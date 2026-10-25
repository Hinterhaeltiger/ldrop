// SPDX-License-Identifier: GPL-3.0-or-later
//! UKEY2 key exchange, D2D key derivation, SecureMessage framing and the PIN (SPEC §5.3).
//!
//! UKEY2 per google/ukey2 [K1]; derivation constants and the PIN algorithm per NearDrop
//! PROTOCOL.md [R43] and rquickshare [R42]. Pure functions: no I/O.

use aes::Aes256;
use cbc::cipher::block_padding::Pkcs7;
use cbc::cipher::{BlockModeDecrypt, BlockModeEncrypt, KeyIvInit};
use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use p256::elliptic_curve::sec1::ToSec1Point;
use p256::{PublicKey, SecretKey};
use prost::Message;
use rand::Rng;
use sha2::{Digest, Sha256, Sha512};

use crate::Error;
use crate::proto::location::nearby::connections::OfflineFrame;
use crate::proto::securegcm::ukey2_alert::AlertType;
use crate::proto::securegcm::ukey2_client_init::CipherCommitment;
use crate::proto::securegcm::{
    DeviceToDeviceMessage, GcmMetadata, Type as GcmType, Ukey2Alert, Ukey2ClientFinished, Ukey2ClientInit,
    Ukey2HandshakeCipher, Ukey2Message, Ukey2ServerInit, ukey2_message,
};
use crate::proto::securemessage::{
    EcP256PublicKey, EncScheme, GenericPublicKey, Header, HeaderAndBody, PublicKeyType, SecureMessage, SigScheme,
};

type HmacSha256 = Hmac<Sha256>;
type Aes256CbcEnc = cbc::Encryptor<Aes256>;
type Aes256CbcDec = cbc::Decryptor<Aes256>;

/// The only next protocol Quick Share uses.
const NEXT_PROTOCOL: &str = "AES_256_CBC-HMAC_SHA256";
/// SHA-256("D2D").
const D2D_SALT: [u8; 32] = hex32("82AA55A0D397F88346CA1CEE8D3909B95F13FA7DEB1D4AB38376B8256DA85510");
/// SHA-256("SecureMessage").
const SECURE_MESSAGE_SALT: [u8; 32] = hex32("BF9D2A53C63616D75DB0A7165B91C1EF73E537F2427405FA23610A4BE657642E");
const UKEY2_VERSION: i32 = 1;

/// A handshake failure, with the alert to send the peer when there is one.
#[derive(Debug)]
pub struct HandshakeError {
    pub alert: Option<AlertType>,
    pub reason: String,
}

impl HandshakeError {
    fn new(alert: AlertType, reason: impl Into<String>) -> Self {
        HandshakeError { alert: Some(alert), reason: reason.into() }
    }

    fn silent(reason: impl Into<String>) -> Self {
        HandshakeError { alert: None, reason: reason.into() }
    }

    /// Serialized `Ukey2Message` carrying the alert.
    pub fn alert_message(&self) -> Option<Vec<u8>> {
        let alert = self.alert?;
        let body = Ukey2Alert { r#type: Some(alert as i32), error_message: Some(self.reason.clone()) };
        Some(
            Ukey2Message {
                message_type: Some(ukey2_message::Type::Alert as i32),
                message_data: Some(body.encode_to_vec()),
            }
            .encode_to_vec(),
        )
    }
}

impl From<HandshakeError> for Error {
    fn from(e: HandshakeError) -> Self {
        Error::Protocol(format!("UKEY2: {}", e.reason))
    }
}

/// Keys for one direction of the SecureMessage channel.
#[derive(Clone)]
pub struct DirectionKeys {
    aes: [u8; 32],
    hmac: [u8; 32],
}

/// Outcome of the key exchange.
pub struct Session {
    /// UKEY2 authentication string; the PIN is derived from it.
    pub auth: [u8; 32],
    pub sealer: Sealer,
    pub opener: Opener,
}

impl Session {
    pub fn pin(&self) -> String {
        pin(&self.auth)
    }
}

/// Client side: sends ClientInit, receives ServerInit, sends ClientFinish.
pub struct ClientHandshake {
    secret: SecretKey,
    client_init: Vec<u8>,
    client_finish: Vec<u8>,
}

impl ClientHandshake {
    pub fn new() -> Self {
        let secret = random_secret_key();
        let finished = Ukey2ClientFinished { public_key: Some(encode_public_key(&secret.public_key())) };
        let client_finish = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientFinish as i32),
            message_data: Some(finished.encode_to_vec()),
        }
        .encode_to_vec();
        let init = Ukey2ClientInit {
            version: Some(UKEY2_VERSION),
            random: Some(rand::rng().random::<[u8; 32]>().to_vec()),
            cipher_commitments: vec![CipherCommitment {
                handshake_cipher: Some(Ukey2HandshakeCipher::P256Sha512 as i32),
                commitment: Some(Sha512::digest(&client_finish).to_vec()),
            }],
            next_protocol: Some(NEXT_PROTOCOL.to_owned()),
        };
        let client_init = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientInit as i32),
            message_data: Some(init.encode_to_vec()),
        }
        .encode_to_vec();
        ClientHandshake { secret, client_init, client_finish }
    }

    /// Serialized ClientInit message, to send first.
    pub fn client_init(&self) -> &[u8] {
        &self.client_init
    }

    /// Processes the raw ServerInit; returns the ClientFinish to send and the session keys.
    pub fn finish(self, server_init_raw: &[u8]) -> Result<(Vec<u8>, Session), HandshakeError> {
        let msg = Ukey2Message::decode(server_init_raw)
            .map_err(|_| HandshakeError::new(AlertType::BadMessage, "ServerInit does not parse"))?;
        if msg.message_type() == ukey2_message::Type::Alert {
            let alert = Ukey2Alert::decode(msg.message_data()).unwrap_or_default();
            return Err(HandshakeError::silent(format!("peer sent alert {:?}", alert.r#type())));
        }
        if msg.message_type() != ukey2_message::Type::ServerInit {
            return Err(HandshakeError::new(AlertType::BadMessageType, "expected ServerInit"));
        }
        let init = Ukey2ServerInit::decode(msg.message_data())
            .map_err(|_| HandshakeError::new(AlertType::BadMessageData, "ServerInit body does not parse"))?;
        if init.version() != UKEY2_VERSION {
            return Err(HandshakeError::new(AlertType::BadVersion, "unsupported UKEY2 version"));
        }
        if init.random().len() != 32 {
            return Err(HandshakeError::new(AlertType::BadRandom, "ServerInit random must be 32 bytes"));
        }
        if init.handshake_cipher() != Ukey2HandshakeCipher::P256Sha512 {
            return Err(HandshakeError::new(AlertType::BadHandshakeCipher, "server chose an unsupported cipher"));
        }
        let peer = decode_public_key(init.public_key())?;
        let (auth, next) = derive_secrets(&self.secret, &peer, &self.client_init, server_init_raw);
        let (client, server) = d2d_keys(&next);
        Ok((self.client_finish, Session { auth, sealer: Sealer::new(client), opener: Opener::new(server) }))
    }
}

impl Default for ClientHandshake {
    fn default() -> Self {
        Self::new()
    }
}

/// Server side, after receiving ClientInit and sending ServerInit.
pub struct ServerHandshake {
    secret: SecretKey,
    client_init: Vec<u8>,
    server_init: Vec<u8>,
    commitment: Vec<u8>,
}

impl ServerHandshake {
    /// Validates the raw ClientInit and builds the ServerInit to send back.
    pub fn respond(client_init_raw: &[u8]) -> Result<Self, HandshakeError> {
        let msg = Ukey2Message::decode(client_init_raw)
            .map_err(|_| HandshakeError::new(AlertType::BadMessage, "ClientInit does not parse"))?;
        if msg.message_type() != ukey2_message::Type::ClientInit {
            return Err(HandshakeError::new(AlertType::BadMessageType, "expected ClientInit"));
        }
        let init = Ukey2ClientInit::decode(msg.message_data())
            .map_err(|_| HandshakeError::new(AlertType::BadMessageData, "ClientInit body does not parse"))?;
        if init.version() != UKEY2_VERSION {
            return Err(HandshakeError::new(AlertType::BadVersion, "unsupported UKEY2 version"));
        }
        if init.random().len() != 32 {
            return Err(HandshakeError::new(AlertType::BadRandom, "ClientInit random must be 32 bytes"));
        }
        let commitment = init
            .cipher_commitments
            .iter()
            .find(|c| c.handshake_cipher() == Ukey2HandshakeCipher::P256Sha512)
            .map(|c| c.commitment().to_vec())
            .ok_or_else(|| HandshakeError::new(AlertType::BadHandshakeCipher, "client offers no P256_SHA512"))?;
        if init.next_protocol() != NEXT_PROTOCOL {
            return Err(HandshakeError::new(
                AlertType::BadNextProtocol,
                format!("unsupported next protocol {:?}", init.next_protocol()),
            ));
        }
        let secret = random_secret_key();
        let server_init = Ukey2Message {
            message_type: Some(ukey2_message::Type::ServerInit as i32),
            message_data: Some(
                Ukey2ServerInit {
                    version: Some(UKEY2_VERSION),
                    random: Some(rand::rng().random::<[u8; 32]>().to_vec()),
                    handshake_cipher: Some(Ukey2HandshakeCipher::P256Sha512 as i32),
                    public_key: Some(encode_public_key(&secret.public_key())),
                }
                .encode_to_vec(),
            ),
        }
        .encode_to_vec();
        Ok(ServerHandshake { secret, client_init: client_init_raw.to_vec(), server_init, commitment })
    }

    /// Serialized ServerInit message, to send.
    pub fn server_init(&self) -> &[u8] {
        &self.server_init
    }

    /// Verifies the raw ClientFinish against the commitment and derives the session keys.
    pub fn finish(self, client_finish_raw: &[u8]) -> Result<Session, HandshakeError> {
        // Constant-time compare is not needed: the commitment is public once the message is sent.
        if Sha512::digest(client_finish_raw).as_slice() != self.commitment.as_slice() {
            return Err(HandshakeError::new(AlertType::BadMessage, "ClientFinish does not match the commitment"));
        }
        let msg = Ukey2Message::decode(client_finish_raw)
            .map_err(|_| HandshakeError::new(AlertType::BadMessage, "ClientFinish does not parse"))?;
        if msg.message_type() != ukey2_message::Type::ClientFinish {
            return Err(HandshakeError::new(AlertType::BadMessageType, "expected ClientFinish"));
        }
        let finished = Ukey2ClientFinished::decode(msg.message_data())
            .map_err(|_| HandshakeError::new(AlertType::BadMessageData, "ClientFinish body does not parse"))?;
        let peer = decode_public_key(finished.public_key())?;
        let (auth, next) = derive_secrets(&self.secret, &peer, &self.client_init, &self.server_init);
        let (client, server) = d2d_keys(&next);
        Ok(Session { auth, sealer: Sealer::new(server), opener: Opener::new(client) })
    }
}

/// Encrypts and signs outgoing offline frames.
pub struct Sealer {
    keys: DirectionKeys,
    seq: i32,
}

impl Sealer {
    fn new(keys: DirectionKeys) -> Self {
        Sealer { keys, seq: 0 }
    }

    /// Serialized `SecureMessage` carrying `frame` with the next sequence number.
    pub fn seal(&mut self, frame: &OfflineFrame) -> Vec<u8> {
        self.seq = self.seq.wrapping_add(1);
        let d2d = DeviceToDeviceMessage { message: Some(frame.encode_to_vec()), sequence_number: Some(self.seq) };
        let iv: [u8; 16] = rand::rng().random();
        let body =
            Aes256CbcEnc::new(&self.keys.aes.into(), &iv.into()).encrypt_padded_vec::<Pkcs7>(&d2d.encode_to_vec());
        let header_and_body = HeaderAndBody {
            header: Header {
                signature_scheme: SigScheme::HmacSha256 as i32,
                encryption_scheme: EncScheme::Aes256Cbc as i32,
                iv: Some(iv.to_vec()),
                public_metadata: Some(
                    GcmMetadata { r#type: GcmType::DeviceToDeviceMessage as i32, version: Some(1) }.encode_to_vec(),
                ),
                ..Default::default()
            },
            body,
        }
        .encode_to_vec();
        let mut mac = <HmacSha256 as KeyInit>::new_from_slice(&self.keys.hmac).expect("HMAC takes any key length");
        mac.update(&header_and_body);
        SecureMessage { header_and_body, signature: mac.finalize().into_bytes().to_vec() }.encode_to_vec()
    }
}

/// Verifies and decrypts incoming `SecureMessage`s, enforcing strict sequence numbers.
pub struct Opener {
    keys: DirectionKeys,
    seq: i32,
}

impl Opener {
    fn new(keys: DirectionKeys) -> Self {
        Opener { keys, seq: 0 }
    }

    pub fn open(&mut self, raw: &[u8]) -> Result<OfflineFrame, Error> {
        let msg = SecureMessage::decode(raw).map_err(|_| Error::protocol("SecureMessage does not parse"))?;
        let mut mac = <HmacSha256 as KeyInit>::new_from_slice(&self.keys.hmac).expect("HMAC takes any key length");
        mac.update(&msg.header_and_body);
        mac.verify_slice(&msg.signature).map_err(|_| Error::Integrity)?;

        let hb = HeaderAndBody::decode(msg.header_and_body.as_slice())
            .map_err(|_| Error::protocol("HeaderAndBody does not parse"))?;
        if hb.header.encryption_scheme != EncScheme::Aes256Cbc as i32
            || hb.header.signature_scheme != SigScheme::HmacSha256 as i32
        {
            return Err(Error::protocol("unexpected SecureMessage schemes"));
        }
        let iv: [u8; 16] =
            hb.header.iv().try_into().map_err(|_| Error::protocol("SecureMessage IV must be 16 bytes"))?;
        let plain = Aes256CbcDec::new(&self.keys.aes.into(), &iv.into())
            .decrypt_padded_vec::<Pkcs7>(&hb.body)
            .map_err(|_| Error::Integrity)?;
        let d2d = DeviceToDeviceMessage::decode(plain.as_slice())
            .map_err(|_| Error::protocol("DeviceToDeviceMessage does not parse"))?;
        let expected = self.seq.wrapping_add(1);
        if d2d.sequence_number() != expected {
            return Err(Error::protocol(format!("sequence number {} (expected {expected})", d2d.sequence_number())));
        }
        self.seq = expected;
        OfflineFrame::decode(d2d.message()).map_err(|_| Error::protocol("encrypted offline frame does not parse"))
    }
}

/// Chromium's 4-digit PIN from the UKEY2 authentication string [R43][K2].
pub fn pin(auth: &[u8]) -> String {
    const MODULO: i32 = 9973;
    const MULTIPLIER: i32 = 31;
    let mut hash: i32 = 0;
    let mut multiplier: i32 = 1;
    for &b in auth {
        hash = (hash + i32::from(b as i8) * multiplier) % MODULO;
        multiplier = (multiplier * MULTIPLIER) % MODULO;
    }
    format!("{:04}", hash.abs())
}

fn random_secret_key() -> SecretKey {
    loop {
        // Fails only for zero or values ≥ the group order (probability ~2^-32).
        if let Ok(key) = SecretKey::from_slice(&rand::rng().random::<[u8; 32]>()) {
            return key;
        }
    }
}

/// `GenericPublicKey` with x and y as big-endian two's-complement integers, as Java's
/// `BigInteger.toByteArray()` writes them: a leading zero byte when the top bit is set.
fn encode_public_key(key: &PublicKey) -> Vec<u8> {
    let point = key.to_sec1_point(false);
    let signed = |coord: &[u8]| {
        let mut v = Vec::with_capacity(33);
        if coord[0] & 0x80 != 0 {
            v.push(0);
        }
        v.extend_from_slice(coord);
        v
    };
    GenericPublicKey {
        r#type: PublicKeyType::EcP256 as i32,
        ec_p256_public_key: Some(EcP256PublicKey {
            x: signed(point.x().expect("uncompressed point")),
            y: signed(point.y().expect("uncompressed point")),
        }),
        ..Default::default()
    }
    .encode_to_vec()
}

fn decode_public_key(raw: &[u8]) -> Result<PublicKey, HandshakeError> {
    let bad = |why: &str| HandshakeError::new(AlertType::BadPublicKey, why.to_owned());
    let key = GenericPublicKey::decode(raw).map_err(|_| bad("public key does not parse"))?;
    if key.r#type != PublicKeyType::EcP256 as i32 {
        return Err(bad("public key is not EC P-256"));
    }
    let ec = key.ec_p256_public_key.ok_or_else(|| bad("missing EC P-256 key"))?;
    let mut sec1 = [0u8; 65];
    sec1[0] = 0x04;
    let (x, y) = sec1[1..].split_at_mut(32);
    for (coord, dst) in [(&ec.x, x), (&ec.y, y)] {
        // Strip the sign byte(s); what remains must fit 32 bytes and is left-padded.
        let start = coord.iter().position(|&b| b != 0).unwrap_or(coord.len());
        let magnitude = &coord[start..];
        if magnitude.len() > 32 {
            return Err(bad("public key coordinate too long"));
        }
        dst[32 - magnitude.len()..].copy_from_slice(magnitude);
    }
    PublicKey::from_sec1_bytes(&sec1).map_err(|_| bad("public key is not on the curve"))
}

/// Returns (authentication string, next-protocol secret) [K1].
fn derive_secrets(
    secret: &SecretKey,
    peer: &PublicKey,
    client_init: &[u8],
    server_init: &[u8],
) -> ([u8; 32], [u8; 32]) {
    let shared = p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), peer.as_affine());
    let ikm = Sha256::digest(shared.raw_secret_bytes());
    let info = [client_init, server_init].concat();
    (hkdf(b"UKEY2 v1 auth", &ikm, &info), hkdf(b"UKEY2 v1 next", &ikm, &info))
}

/// Returns (client keys, server keys); each side sends with its own and receives with the other.
fn d2d_keys(next_secret: &[u8; 32]) -> (DirectionKeys, DirectionKeys) {
    let side = |info: &[u8]| {
        let d2d = hkdf(&D2D_SALT, next_secret, info);
        DirectionKeys {
            aes: hkdf(&SECURE_MESSAGE_SALT, &d2d, b"ENC:2"),
            hmac: hkdf(&SECURE_MESSAGE_SALT, &d2d, b"SIG:1"),
        }
    };
    (side(b"client"), side(b"server"))
}

fn hkdf(salt: &[u8], ikm: &[u8], info: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    Hkdf::<Sha256>::new(Some(salt), ikm).expand(info, &mut out).expect("32 bytes is a valid HKDF length");
    out
}

const fn hex32(s: &str) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            b'A'..=b'F' => c - b'A' + 10,
            b'a'..=b'f' => c - b'a' + 10,
            _ => panic!("not hex"),
        }
    }
    let b = s.as_bytes();
    assert!(b.len() == 64);
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = nibble(b[2 * i]) << 4 | nibble(b[2 * i + 1]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::location::nearby::connections::{KeepAliveFrame, V1Frame, v1_frame};

    fn handshake() -> (Session, Session) {
        let client = ClientHandshake::new();
        let server = ServerHandshake::respond(client.client_init()).unwrap();
        let server_init = server.server_init().to_vec();
        let (finish, client_session) = client.finish(&server_init).unwrap();
        let server_session = server.finish(&finish).unwrap();
        (client_session, server_session)
    }

    fn keep_alive() -> OfflineFrame {
        OfflineFrame {
            version: Some(1),
            v1: Some(V1Frame {
                r#type: Some(v1_frame::FrameType::KeepAlive as i32),
                keep_alive: Some(KeepAliveFrame { ack: Some(false), seq_num: None }),
                ..Default::default()
            }),
        }
    }

    #[test]
    fn salts_are_the_published_hashes() {
        assert_eq!(D2D_SALT.as_slice(), Sha256::digest(b"D2D").as_slice());
        assert_eq!(SECURE_MESSAGE_SALT.as_slice(), Sha256::digest(b"SecureMessage").as_slice());
    }

    #[test]
    fn both_sides_agree() {
        let (mut client, mut server) = handshake();
        assert_eq!(client.auth, server.auth);
        assert_eq!(client.pin(), server.pin());
        assert_eq!(client.pin().len(), 4);
        for _ in 0..3 {
            let sealed = client.sealer.seal(&keep_alive());
            assert_eq!(server.opener.open(&sealed).unwrap(), keep_alive());
            let sealed = server.sealer.seal(&keep_alive());
            assert_eq!(client.opener.open(&sealed).unwrap(), keep_alive());
        }
    }

    #[test]
    fn rejects_tampering_replay_and_reordering() {
        let (mut client, mut server) = handshake();
        let first = client.sealer.seal(&keep_alive());
        let second = client.sealer.seal(&keep_alive());

        let mut tampered = first.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(matches!(server.opener.open(&tampered), Err(Error::Integrity)));

        assert!(server.opener.open(&second).is_err(), "out of order");
        // The failed attempt did not advance the counter.
        server.opener.open(&first).unwrap();
        assert!(server.opener.open(&first).is_err(), "replay");
        server.opener.open(&second).unwrap();
    }

    #[test]
    fn server_checks_the_commitment() {
        let client = ClientHandshake::new();
        let server = ServerHandshake::respond(client.client_init()).unwrap();
        // A ClientFinish from a different client does not match this client's commitment.
        let (other_finish, _) = ClientHandshake::new().finish(server.server_init()).unwrap();
        let err = server.finish(&other_finish).err().unwrap();
        assert_eq!(err.alert, Some(AlertType::BadMessage));
    }

    #[test]
    fn server_rejects_bad_client_init() {
        let err = ServerHandshake::respond(b"garbage").err().unwrap();
        assert!(err.alert_message().is_some());
        let init = Ukey2ClientInit {
            version: Some(1),
            random: Some(vec![0; 32]),
            cipher_commitments: vec![CipherCommitment {
                handshake_cipher: Some(Ukey2HandshakeCipher::Curve25519Sha512 as i32),
                commitment: Some(vec![0; 64]),
            }],
            next_protocol: Some(NEXT_PROTOCOL.into()),
        };
        let msg = Ukey2Message {
            message_type: Some(ukey2_message::Type::ClientInit as i32),
            message_data: Some(init.encode_to_vec()),
        };
        let err = ServerHandshake::respond(&msg.encode_to_vec()).err().unwrap();
        assert_eq!(err.alert, Some(AlertType::BadHandshakeCipher));
    }

    #[test]
    fn public_keys_round_trip_with_sign_bytes() {
        for _ in 0..32 {
            let key = random_secret_key().public_key();
            let raw = encode_public_key(&key);
            let generic = GenericPublicKey::decode(raw.as_slice()).unwrap();
            let ec = generic.ec_p256_public_key.unwrap();
            assert!(ec.x.len() == 32 || (ec.x.len() == 33 && ec.x[0] == 0));
            assert_eq!(decode_public_key(&raw).unwrap(), key);
        }
        let off_curve = GenericPublicKey {
            r#type: PublicKeyType::EcP256 as i32,
            ec_p256_public_key: Some(EcP256PublicKey { x: vec![1; 32], y: vec![2; 32] }),
            ..Default::default()
        };
        assert!(decode_public_key(&off_curve.encode_to_vec()).is_err());
    }

    #[test]
    fn pin_matches_reference_algorithm() {
        // Reference: rquickshare `to_four_digit_string` / Chromium `GetFourDigitString`.
        assert_eq!(pin(&[0; 32]), "0000");
        assert_eq!(pin(&[1]), "0001");
        assert_eq!(pin(&[1, 1]), "0032");
        // Signed bytes: 0xFF counts as -1.
        assert_eq!(pin(&[0xFF]), "0001");
        assert_eq!(pin(&[0xFF, 0x02]), "0061");
    }
}
