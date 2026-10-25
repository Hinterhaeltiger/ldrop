# ldrop — Specification: Quick Share + AirDrop interoperability for Linux

> Implementation spec for an agent. Revision 3 (adds GUI + TUI), 2026-10-25.
> Source tags: `[R#]` = looked up while writing this spec. `[K#]` = from prior knowledge, not fetched, so **verify before relying on it**. Full list in §20.
> Research spikes `S0.x` are defined in §16 (milestone M0).

---

## 0. Context

Quick Share on Android can send to and receive from Apple devices through AirDrop, directly and peer-to-peer:
- Pixel 10 since Nov 2025 [R1][R2]; Pixel 9 since Feb 2026 [R5][R11]; Samsung Galaxy S26 reported [R12]. Qualcomm says Snapdragon devices will follow [R13].
- Google built it without Apple, in Rust [R1][R6].
- The Apple device must be set to AirDrop **"Everyone for 10 minutes"** [R1][R3].

**Goal:** a Linux daemon that behaves like Quick Share on a Pixel. One list of share targets shows nearby Android devices (Quick Share protocol) and Apple devices (AirDrop protocol), in both directions, peer-to-peer.

### Decisions already made (by user)
| Topic | Decision |
|---|---|
| Language | **Rust** |
| Wi-Fi hardware | **Built-in laptop Wi-Fi.** No dedicated dongle required (one is optional for development, §3.4). |
| UI | Daemon + CLI + D-Bus API, plus a **Qt/Kirigami GUI** (Rust + cxx-qt) themed natively on Plasma, including **KDE Union**, and following system appearance on every other desktop/WM, plus a **TUI** for window-manager users. Integrations: file-manager menus, tray, KDE share menu, "Open with" (§11). |
| Reuse / license | **Reuse existing code.** Project license **GPL-3.0-or-later**. |

### Decision made by this spec
**AirDrop is built on Owly** [R27], an MIT-licensed Rust project with:
- a userspace AWDL implementation;
- an AirDrop protocol crate;
- a Linux radio backend with per-driver profiles;
- a privileged daemon that hands out `awdl0` and restores Wi-Fi afterwards;
- its own written AirDrop wire specification [R29].

ldrop's own work is Quick Share (including the peer-to-peer links), the unified daemon/API/CLI, consent and UX parity, and integrating Owly. Details and alternatives are in §2.

### Non-goals (v1)
- **AirDrop "Contacts Only".** It needs an Apple-issued identity and a validation record [R29][R37]. Pixel has the same gap [R1]. Owly has an experimental path that reuses an identity from the user's own Mac [R30]; keep it as a post-v1 option only.
- **Quick Share "Contacts" / "Your devices".** These need Google-account certificates.
- NameDrop, AirDrop over Internet, Handoff, non-Linux ports.
- A separate GTK-native frontend. The Qt GUI follows GTK desktops' color scheme, accent and contrast instead (§11.5).

---

## 1. UX contract ("same as Android")

### 1.1 Visibility (receiving)
| Mode | Behavior |
|---|---|
| `hidden` (default) | Not discoverable. Sending still works. |
| `everyone` | Discoverable by nearby Android (Quick Share) and Apple (AirDrop) devices. **Auto-reverts to `hidden` after 10 min** (default 600 s, configurable). |
| `contacts`, `your-devices` | Not supported. The API returns `NotSupported`. |

Visibility is never persisted. After a daemon restart or reboot it is `hidden`.

### 1.2 Send flow
1. The client calls `StartDiscovery` (`ldrop send …` today, a share sheet later).
2. The daemon starts every discovery path that is enabled and supported:
   - **Quick Share:** mDNS browse, BLE wake advert, BLE scan.
   - **AirDrop:** bring up the Apple link through netd, then use Owly's discovery (BLE trigger plus probing AWDL peers) [R30].
3. A merged, de-duplicated device list streams out via `DeviceFound`/`DeviceLost`, carrying name, kind (phone/tablet/laptop) and platform (android/apple).
4. The user picks one or more devices, then calls `Send`. Several targets can run concurrently.
5. While waiting:
   - **Quick Share:** a **4-digit PIN** is shown, and both sides show the same PIN.
   - **AirDrop:** "Waiting…" until the Apple user accepts. Decline or timeout ends as `Rejected` (Apple responds with HTTP 403 [R30]).
6. Progress, then Completed / Rejected / Cancelled / Failed. Either side can cancel at any time.
7. When the session closes, all discovery and wake adverts stop, and the Apple link is released.

### 1.3 Receive flow
1. An incoming request raises `IncomingRequest` with sender name, platform, item list (names, MIME types, sizes) and, for Quick Share, the PIN.
2. The registered **Agent** (frontend or CLI) shows the prompt. With no agent registered, the daemon posts a freedesktop notification with Accept/Decline actions.
3. **Never auto-accept.** Prompt timeouts:
   - Quick Share: 60 s (configurable).
   - AirDrop: the window is bounded by how long `/Ask` is held open (up to 15 s per [R29]), so the prompt must appear immediately.
4. Files go to the XDG Download dir (configurable). Text and URLs are stored in the transfer record, and the agent decides whether to copy or open them.

### 1.4 Payloads
- Files: any count and size.
- Text and URLs. AirDrop link shares travel inside `/Ask` with no upload [R29].
- Quick Share Wi-Fi credentials: receive only. They are surfaced to the agent and never applied automatically.

### 1.5 QR-code send (Quick Share, Android parity)
Android can share by QR code to a phone that isn't in Everyone mode. ldrop matches this with `ldrop send --qr`:
1. ldrop shows the QR code (terminal render, or via D-Bus for frontends).
2. The phone scans it and advertises with a QR TLV.
3. ldrop matches the TLV and connects.

Protocol from [R43] §QR. This is a milestone M11 feature.

---

## 2. AirDrop implementation approaches (research summary + decision)

| # | Approach | Status / evidence | Verdict |
|---|---|---|---|
| A | **Owly** (t4t5/omdrop-owl): Rust; crates `owly-awdl`, `owly-radio`, `owly-airdrop`, `owlyd`, `owly-cli`; MIT [R27][R28] | Written AirDrop spec with requirement IDs [R29]; conformance tests and veth harness [R32]; hardware matrix with shared and exclusive modes including Intel [R27]; AUR packages `owly`, `omdrop-owl` [R27]; link-share `/Ask` accepted by iOS 26.7.1 [R31]; released Oct 2026 [R35] | **Chosen.** Use it as library crates (preferred) or as an external `owlyd` (fallback), decided in S0.3 |
| B | OWL (C) + OpenDrop (Python), seemoo-lab; GPL-3.0 [R36][R37] | OWL archived; no concurrent AP connection; static election [R36]. OpenDrop lacks the BLE trigger, receives one file only, auto-accepts everything, has no peer authentication [R37] | Reference only |
| C | **Wi-Fi Aware (NAN)** link instead of AWDL | iOS 26 ships Wi-Fi Aware worldwide [R16][R17]. Press claims Pixel uses it [R10][R13][R14][R15], but a user reports Pixel↔iPhone working on iOS 18.5, which has no Wi-Fi Aware [R10]. The Linux NAN data path is still landing in 2026 [R19]–[R26] | **Experimental backend** (§7), gated by S0.1 and kernel support |
| D | Dedicated USB adapter (e.g. MT7612U active monitor, RTL8852BU as a second card [R27]) | Works per Owly's hardware list; leaves built-in Wi-Fi alone | Optional for development/testing; not required (user decision) |
| E | iOS companion app using Apple's Wi-Fi Aware framework [R17] | Needs an app on the iPhone; that isn't AirDrop | Rejected (not Android parity) |
| F | AirDrop-like tools with their own protocols (LocalSend, FlyingCarpet) [R40][R41] | Not AirDrop-compatible | Rejected |

**Link-layer question (ADR-001):** the evidence conflicts on whether Pixel uses AWDL or Wi-Fi Aware [R10][R13][R14]. It **does not block v1**, because Owly proves AWDL works from Linux to current iOS [R27][R31]. S0.1 still settles it, which decides whether §7 gets built.

---

## 3. Hardware model & capability probing

### 3.1 Requirements
- Linux ≥ 6.12 (dev box: Arch, kernel 7.2).
- BlueZ ≥ 5.66 with LE advertising: service data for Quick Share, manufacturer data for AirDrop. ≥ 2 advertising instances preferred.
- Built-in Wi-Fi with an nl80211 driver.

### 3.2 Link backends
| ID | Backend | For | Notes |
|---|---|---|---|
| `lan` | Existing network, mDNS | Android | Any hardware. Baseline. |
| `bt` | Bluetooth Classic RFCOMM | Android | Initial-connection medium for peer-to-peer; slow fallback. |
| `wfd` | Wi-Fi Direct (wpa_supplicant P2P) | Android | Bandwidth-upgrade target. Needs the P2P iftypes. |
| `hotspot` | Laptop AP, or join the phone's AP via NetworkManager | Android | Upgrade target. Needs a STA+AP combination, or temporarily drops Wi-Fi. |
| `awdl-shared` | Owly shared mode | Apple | Wi-Fi stays connected. AP must be on channel 6, 44 or 149 [R27]. |
| `awdl-exclusive` | Owly exclusive mode | Apple | **Wi-Fi is down while active** [R27]. Needs `allow_exclusive_radio = true`. |
| `awdl-activemon` | Owly active-monitor mode | Apple | For cards with neither injection mode [R27]. |
| `nan` | Wi-Fi Aware via nl80211 | Apple (if ADR-001), Android `WIFI_AWARE` | Experimental (§7). |

### 3.3 Hardware status (from Owly's matrix, Oct 2026 [R27]; re-read the live table, which is generated from `cards.toml` [R33])
| Status | Shared mode | Exclusive mode |
|---|---|---|
| Works | MediaTek MT7925 (`mt7925e`); Realtek RTL8852AE/BE/CE (`rtw89_*`, opt-in) | Intel 9560, 9260, AX200, AX201, AX211, BE200, Panther Lake BE (`iwlwifi`); Realtek RTL8822BE; MT7612U (active monitor); RTL8852BU (USB second card) |
| Unreliable | — | MT7921, MT7922 (shared fails); RTL8188FTV (opt-in) |
| Fails | — | MT7927; RTL8821CE; Qualcomm WCN6855, WCN7850; Intel 7265, 8260, 8265 |

Consequences for the "built-in Wi-Fi" decision:
- **Intel laptops (most common):** AirDrop works only in exclusive mode, so Wi-Fi drops while Apple visibility or an Apple transfer is active. On channel 6 it is slow: a 2.4 MB photo took 10–17 s on an AX201 [R27].
- **MT7925 / RTL8852xE:** behaves like a Pixel (Wi-Fi stays up). Throughput is about 1.3 MB/s receiving and 490 KB/s sending [R34][R35].
- **Qualcomm WCN6855/WCN7850:** no AirDrop with today's backends. Android over `lan`/`wfd`/`bt` still works.

### 3.4 Dev environment
The user's desktop has **no Wi-Fi** (Ethernet `enp6s0`) and a Realtek RTL8761B BT dongle (`0bda:8771`).
- Quick Share work over `lan` and `bt`/BLE runs there.
- AirDrop and Wi-Fi P2P work needs a laptop, or a supported USB card (MT7612U or RTL8852BU [R27]), plus `mac80211_hwsim` for CI.

### 3.5 `ldrop doctor`
Reads system state without changing it:
- nl80211: iftypes (monitor, P2P_GO, P2P_CLIENT, AP, NAN, NAN_DATA), interface combinations, driver, association channel, regdom flags for channels 6/44/149.
- The Owly driver profile for the card (`drivers.toml` [R27]).
- BlueZ: adapter, advertising instances, RFCOMM.

`--card-test` wraps Owly's `test-card` flow [R33]; it needs netd. Output: each backend as `supported | degraded | unsupported` with a reason. `--json` is supported.

**Backend selection order:**
- Apple: `nan` (only if enabled and ADR-001 + probe say yes) → `awdl-shared` → `awdl-activemon` → `awdl-exclusive` (only if opted in) → disabled, with a clear message.
- Android: `lan` → `wfd` → `hotspot` → `bt`.

---

## 4. Architecture

```
 ldrop (CLI)    ldrop-tui    ldrop-gui (Qt/Kirigami) + tray + integrations
      └────────────┴─────── ldrop-client ───────┘
                         │
              session D-Bus: org.ldrop.Ldrop1
                         │
                  ldropd (user daemon, unprivileged)
   ┌─────────────────────┼─────────────────────────────┐
   │ ldrop-core: device registry, transfer FSM, visibility timer,
   │             agent + notifications, storage, config
   ├───────────────┬───────────────────────────────────┤
   │ ldrop-quickshare            owly-airdrop (dependency)
   │ (UKEY2, frames, mediums)    wrapped by ldrop-airdrop (consent, progress, storage)
   ├───────────────┴───────────────────────────────────┤
   │ ldrop-ble   ldrop-bt   ldrop-mdns   (BlueZ D-Bus, plain sockets)
   └─────────────────────┬─────────────────────────────┘
              system D-Bus: org.ldrop.NetHelper1 (polkit)
                         │
            ldrop-netd (system service; radio owner + arbiter)
     owly-radio + owly-awdl (dependencies) → awdl0
     ldrop-wifi: nl80211, NetworkManager, wpa_supplicant P2P → wfd / hotspot
     ldrop-nan (experimental)
```

Principles:
- **One radio owner.** `ldrop-netd` decides which of AWDL, Wi-Fi Direct, hotspot or NAN holds the card at any moment. Requests are queued and the arbitration rules are explicit (§13). This is why Owly's crates are embedded rather than running a separate `owlyd` (fallback: §13.4).
- **Privilege separation.** Only netd touches the radio. `ldropd` reaches Apple peers over IPv6 link-local sockets scoped to `awdl0`. Owly's AirDrop layer already runs unprivileged [R27].
- **Protocol crates are I/O-agnostic** state machines, so they can be unit-tested and fuzzed.

### 4.1 Workspace
```
ldrop/
  Cargo.toml                 # workspace, edition 2024, GPL-3.0-or-later, rust ≥ 1.88 (Owly MSRV [R27])
  crates/
    ldrop-core/              # domain model, transfer FSM, config, storage, visibility, agent
    ldrop-quickshare/        # Quick Share (ported from rquickshare core_lib [R42])
    ldrop-airdrop/           # thin adapter over owly-airdrop: consent, progress, storage policy, device mapping
    ldrop-ble/               # BlueZ over zbus: Quick Share adverts/scan (AirDrop BLE comes from Owly)
    ldrop-bt/                # RFCOMM medium (BlueZ ProfileManager1)
    ldrop-wifi/              # nl80211, NetworkManager + wpa_supplicant D-Bus, capability probe
    ldrop-mdns/              # per-interface mDNS responder/browser (Quick Share on LAN)
    ldrop-nan/               # feature "nan", experimental
    ldropd/                  # user daemon (org.ldrop.Ldrop1)
    ldrop-netd/              # privileged helper (org.ldrop.NetHelper1), embeds owly-radio + owly-awdl
    ldrop-cli/               # `ldrop`
    ldrop-client/            # typed zbus proxies, reactive stores, Agent trait (§11.1)
    ldrop-tui/               # ratatui frontend (§11.3)
    ldrop-gui/               # cxx-qt + QML/Kirigami frontend (§11.4); qml/, build.rs
  proto/                     # Quick Share / Nearby .proto (from rquickshare / google/nearby [R44])
  fuzz/                      # cargo-fuzz targets
  packaging/                 # systemd, polkit, D-Bus confs, sysusers, PKGBUILD
  integrations/              # servicemenus, nautilus, nemo, file-manager actions, purpose (C++/CMake),
                             # .desktop, metainfo, icons, autostart (§11.6)
  docs/research/             # M0 outputs, ADRs, fixtures
  tests/                     # netns integration, hwsim, HIL checklists
```
Owly crates come in as git dependencies pinned to a commit. If they need changes, fork them under `vendor/owly` (MIT allows this) and send the changes upstream.

### 4.2 Crates
tokio, zbus (also for BlueZ, instead of bluer: no libdbus dependency), rustls + tokio-rustls, rcgen, hyper, prost/prost-build, RustCrypto (p256, hkdf, sha2, aes, cbc, hmac), neli or netlink-packet-generic, rtnetlink, mdns-sd (or a custom responder if per-interface binding isn't enough), qrcode (terminal QR), clap, serde + toml, tracing, nix. Frontends: cxx-qt, cxx-qt-lib, cxx-qt-build, ratatui, crossterm. Qt 6 QtQuick and KF6 come in as system libraries: Kirigami, KStatusNotifierItem, KNotifications, KI18n, KColorScheme. Plus Owly's own dependencies. `cargo deny` checks licenses and advisories.

---

## 5. Quick Share protocol (Android side)

Port from **rquickshare `core_lib`** (Rust, GPL-3.0; LAN + BLE wake only) [R42]. Byte-level reference: **NearDrop `PROTOCOL.md`** [R43]. Mediums and protos: **google/nearby** [R44]. Anything marked *verify* is checked in S0.4.

### 5.1 Identity
- Service ID `NearbySharing`. Its hash is the first 3 bytes of SHA-256: `FC 9F 5E` [R43].
- Endpoint ID: 4 random alphanumeric chars, new for each visibility session [R43].
- **Endpoint info** [R43]:
  - Byte 0 holds, in order: version (3 bits, 0 or 1), visibility (1 bit, 0 = visible), device type (3 bits: 0 unknown, 1 phone, 2 tablet, **3 laptop**, 4 car, 5 foldable, 6 XR) and a reserved bit (0). Exact bit positions: *verify* against rquickshare's encoder.
  - Bytes 1–16: 2-byte salt + 14-byte encrypted metadata key. Random is accepted.
  - Then a 1-byte length and the UTF-8 name, present only when visible.
  - Then optional TLVs: type 1 = QR-code data, type 2 = vendor ID (1 = Samsung).

### 5.2 Discovery
- **Receiving on LAN:** mDNS type `_FC9F5ED42C8A._tcp.`, empty domain, any TCP port [R43].
  - Instance name: URL-safe base64 of 10 bytes, `0x23 | endpoint_id(4) | FC 9F 5E | 00 00`.
  - TXT `n` = URL-safe base64 of the endpoint info.
- **Sending on LAN:** browse that type and decode it.
- **BLE wake (sending):** service UUID `0xFE2C`, service data `fc 12 8e 01 42 00 00 00 00 00 00 00 00 00` followed by 10 random bytes. Send it periodically while searching [R43]. It makes Android receivers in Everyone mode publish mDNS.
- **Peer-to-peer BLE discovery** (no shared LAN): the Nearby Connections BLE advertising format comes from google/nearby [R44] (*S0.4*).
- **Bluetooth classic** (no shared LAN, implemented as Nearby does it on Linux/ChromeOS; formats in `docs/research/quickshare-upgrade.md` §1):
  - *Receiving:* while visible, the adapter's alias becomes the encoded device name (web-safe base64 of `0x23 | endpoint_id | FC 9F 5E | flags | 6 × 00 | len | endpoint info`) and the adapter turns discoverable with no timeout. An RFCOMM service under UUID `a82efa21-ae5c-3dde-9bbc-f16da7b16c5a` (BlueZ `ProfileManager1`, no pairing or authorization) takes connections. A legacy BLE advertisement (service data `0xFEF3` = `QCAAABACAAADAAAAAAAAAAA`) makes Android resume its paused classic scan. The previous alias and discoverability go to `$XDG_STATE_HOME/ldrop/bt-adapter.json` first and are put back when hidden, at exit, or on the next start after a crash.
  - *Sending:* a BlueZ scan (classic + LE) reads device names; names that decode as Quick Share devices are listed (medium `bt`), merged by endpoint ID with the same device found over mDNS. Sending prefers the LAN and falls back to RFCOMM (`Device1.ConnectProfile`).
  - *Implementation note (M5):* the endpoint is announced on Bluetooth with the same endpoint ID as on mDNS. Phones are found from their classic names only; parsing their BLE advertisements is implemented but not yet used for discovery (the classic MAC is needed to connect). Transfers stay on Bluetooth until the Wi-Fi upgrade (§5.4) lands.
- **QR mode:** see §5.6.

### 5.3 Connection & handshake [R43]
1. **Transport.** TCP (LAN, Wi-Fi Direct, hotspot) or RFCOMM (BT). Each protobuf is prefixed with a 4-byte big-endian length.
2. **Connection request.** The client sends a plaintext `OfflineFrame` CONNECTION_REQUEST. It carries the raw endpoint info and the client's supported mediums.
3. **UKEY2.** ClientInit → ServerInit → ClientFinish. Keep the raw serialized messages for key derivation, and verify the commitment. Spec and test vectors: [K1].
4. **Key derivation.** HKDF-SHA256 from the next-protocol secret:
   - D2D client/server keys: salt `82AA55A0D397F88346CA1CEE8D3909B95F13FA7DEB1D4AB38376B8256DA85510` (SHA256("D2D")), info `client`/`server`, 32 bytes.
   - Working keys: salt `BF9D2A53C63616D75DB0A7165B91C1EF73E537F2427405FA23610A4BE657642E` (SHA256("SecureMessage")), info `ENC:2` for the encryption key and `SIG:1` for the HMAC key.
   - From the server's view, the decrypt and receive-HMAC keys come from the client key, and the encrypt and send-HMAC keys from the server key. The client swaps them.
5. **Plaintext connection response.** After the key exchange, both sides send a plaintext connection-response frame. Everything after this is encrypted.
6. **SecureMessage.** `HeaderAndBody` carries AES-256-CBC (PKCS7, random 16-byte IV) and HMAC-SHA256, with metadata version 1, type `DEVICE_TO_DEVICE_MESSAGE`. The body is a `DeviceToDeviceMessage`: sequence number starting at 1, separate per direction, enforced strictly. Wrapped inside it is the offline frame.
7. **PIN.** Not the UKEY2 auth string directly; a Chromium-specific 4-digit derivation [R43][K2]. Reference: `h=0, m=1; for b in auth_key as i8: h=(h+b*m)%9973; m=(m*31)%9973; pin=format!("{:04}",h.abs())`. *Verify* against rquickshare/Chromium [R42][K2].
8. **Sharing exchange.**
   - `PairedKeyEncryption`: random 6-byte `secretIDHash` and random 72-byte `signedData` work [R43].
   - `PairedKeyResult(UNABLE)` in both directions.
   - The client sends an `Introduction` (files with payload IDs, text, Wi-Fi metadata).
   - The server answers `ACCEPT`, `REJECT`, `NOT_ENOUGH_SPACE`, … Use `NOT_ENOUGH_SPACE` when the free-space check fails.
9. **Payload.** `PAYLOAD_TRANSFER` frames with a header (`id`, `type` BYTES/FILE, `totalSize`) and chunks (`offset`, `flags` bit 0 = LAST_CHUNK, `body`). Android sends each negotiation message as the full message followed by a zero-length LAST_CHUNK frame; mirror this [R43]. Sender chunks are 512 KiB.
10. **Keep-alive.** A `KEEP_ALIVE` frame every 10 s from both sides is **mandatory**; Android drops the connection without it, and TCP ACKs don't count [R43].
11. **End.** Send `Disconnection`, then close. Handle `Cancel`/`Disconnection` in any state.

### 5.4 Mediums & bandwidth upgrade (peer-to-peer without a shared LAN)
- The client lists its mediums in the connection request. The server intersects them with its own and picks one. After accept it may send a `BANDWIDTH_UPGRADE_NEGOTIATION` frame with credentials [R43].
- NearDrop never worked out the switch-over sequence [R43]. **S0.4 must derive it from google/nearby** [R44]: the upgrade-path frames, client introduction, and the prior-channel close handshake. Output: `docs/research/quickshare-upgrade.md`.
- Mediums ldrop offers, in order: `lan` (same subnet) → `wfd` (Linux as group owner) → `hotspot` (only if the interface combination allows STA+AP; otherwise opt-in like exclusive mode) → stay on `bt`.
- When the phone offers hotspot or Wi-Fi Direct credentials, netd joins with a **temporary, non-autoconnect** NetworkManager profile. Afterwards it deletes the profile and restores the previous connection, including after a crash.
- *Implementation note (M5):*
  - Only connections that start on Bluetooth are upgraded; LAN connections are already fast.
  - **Wi-Fi LAN:** offered when the peer's IP (from its connection request) is in one of our IPv4 subnets; joined whenever the phone offers it. The listener uses `quickshare.tcp_port + 1` when the port is pinned, so the firewall rule is predictable.
  - **Hotspot / Wi-Fi Direct** go through NetworkManager from ldropd itself (`ldrop-wifi::nm`); the separate netd is deferred to M6, where the Apple link needs root anyway. Profiles use `persist = volatile` and `bind-activation = dbus-client`, so a crash takes the network down and deletes the profile; the previous connection is re-activated when the session ends. Joining is a WPA2 client with `hidden`, `never-default` and IPv4 only; hosting is a 2.4 GHz WPA2 AP with `ipv4.method shared` (needs dnsmasq), SSID `DIRECT-xx-ldrop`, random 16-character passphrase.
  - Both interrupt the current Wi-Fi connection on cards without a separate AP interface, so they need `quickshare.allow_wifi_switch = true` unless the card is not connected. NetworkManager's polkit rules allow this from an active local session, not from ssh.
  - ldrop does not host Wi-Fi Direct groups (not offered), and joins Wi-Fi Direct groups as a plain WPA2 client.

### 5.5 Samsung
Samsung's Quick Share has a vendor-ID TLV [R43] and has broken rquickshare before [R42]. Add Samsung to the HIL matrix (§17).

### 5.6 QR mode [R43]
1. **QR URL.** `https://quickshare.google/qrcode#key=<b64url>`. The key is `00 00`, then a version byte (`02`/`03`), then the X coordinate of a compressed ECDSA public key.
2. **Derived values.** HKDF-SHA256 with IKM = key and an empty salt:
   - advertising token: info `advertisingContext`, 16 bytes;
   - name key: info `encryptionKey`, 16 bytes.
3. **Matching the receiver.**
   - A visible receiver puts the token in TLV 1.
   - A hidden receiver puts IV ‖ AES-GCM ciphertext ‖ tag (AAD = token) there.
   - The sender matches the token, otherwise tries decryption.
4. **Skipping the prompt.** Put a P1363 signature over the UKEY2 auth key, made with the QR key, into `qr_code_handshake_data`. NearDrop notes one case where the prompt still appears.

---

## 6. AirDrop integration (Owly)

### 6.1 Protocol overview (design level; normative details in Owly SPEC [R29])
Stack: BLE trigger → AWDL link → IPv6 link-local → mDNS/DNS-SD → TLS → HTTP/1.1 → binary plist → compression → CPIO.
- **Discover:** visibility check.
- **Ask:** consent. The receiver holds the request up to 15 s; a decline or timeout returns `403` and the connection closes [R29][R30].
- **Upload:** compressed archive. Link shares skip it [R29].
- **TLS:** encrypts but does not authenticate peers in Everyone mode [R29].
- **Ports:** Owly's receiver listens on 8771; Apple devices listen on 8770 [R30].
- **TXT flags** `0x089`, no DVZip, so Apple senders use gzip [R30].
- **Discovery quirks:** iPhone mDNS doesn't reach a Linux `awdl0`. Owly's sender therefore also tries `/Discover` at every AWDL peer while advertising over BLE, and sends ICMPv6 echoes every 3 s so Apple peers learn the neighbor [R30].

### 6.2 What ldrop uses from Owly
| Owly crate | Used in | Purpose |
|---|---|---|
| `owly-radio` | ldrop-netd | Card handling, shared/fixed placement, driver profiles (`drivers.toml`, override `/etc/owly/drivers.toml`), firewall probe [R27][R30] |
| `owly-awdl` | ldrop-netd | AWDL; delivers packets to the kernel stack via `awdl0` [R27][R30] |
| `owly-airdrop` | ldropd (via `ldrop-airdrop`) | AirDrop sender + receiver, BLE trigger, discovery quirks (§6.1) |
| `owlyd`, `owly-cli` | reference | Wi-Fi restore and lifecycle logic to mirror in netd; `doctor`/`test-card` flows [R27][R33] |

### 6.3 What `ldrop-airdrop` must add (upstream to Owly when generic)
- **Consent hook.** `/Ask` must call ldrop's agent and return accept or `403` within the hold window. No auto-save.
- **Progress events** per item, plus **cancel** from both sides.
- **Storage policy** from §9 (staging, collisions, limits). Owly SPEC already requires staging dirs, size/depth limits and name validation [R29]; ldrop maps them to its config.
- **Multi-file and link shares** in both directions, with MIME↔UTI mapping.
- **Device mapping** to ldrop's `Device`. Apple addresses are randomized, so Apple identity is never persisted [R34].
- **Identity:** device name from config; model string chosen so iOS shows a laptop icon (S0.2).

### 6.4 Owly open questions that affect ldrop [R31]
- Does BLE advertisement **v1** still wake current iPhones? The meaning of v3 fields is open (#2).
- Contacts-Only details (#7, #10, #13): out of scope.
- TLV layouts that may explain the mDNS gap (#11); whether the ICMPv6 echo is needed (#16).
- `TransferVersion` and the pipelined-upload flag (#4, #6), relevant to throughput.
- macOS receiver `/Ask` form (#15). This is relevant because Macs are AirDrop targets too.

Track these in `docs/research/owly-open-questions.md` and re-check them at each Owly bump.

---

## 7. Wi-Fi Aware backend (`ldrop-nan`, experimental, feature `nan`)
- In scope only if ADR-001 (S0.1) finds Apple AirDrop accepting Wi-Fi Aware peers, or if Android's `WIFI_AWARE` medium is worth supporting.
- Kernel work in 2026 [R19]–[R26]:
  - cfg80211/nl80211 NAN Data Path [R19][R20];
  - mac80211 NAN [R21][R25];
  - iwlwifi-mld NAN data [R22][R23];
  - wireless-next NAN series [R24];
  - hwsim NAN code exists (CVE fix) [R26].

  **The agent must confirm which of these are in the target kernel release.** `ldrop doctor` gates the backend on NAN/NAN_DATA iftypes actually being present.
- Contract: one link-local IPv6 interface plus peer events, identical to the AWDL path, so `ldrop-airdrop` stays link-agnostic.

---

## 8. Device model & discovery merge
```rust
struct Device {
    id: DeviceId,          // opaque; stable only within one discovery session
    name: String,
    kind: DeviceKind,      // Phone | Tablet | Laptop | Desktop | Car | Xr | Unknown
    platform: Platform,    // Android | Apple
    protocol: Protocol,    // QuickShare | AirDrop
    mediums: Vec<Medium>,  // Lan, Bt, Ble, Awdl, Nan
    vendor: Option<Vendor>,// e.g. Samsung (Quick Share TLV 2)
    last_seen: Instant,
}
```
- Merge Quick Share peers seen over mDNS and BLE by endpoint ID. Apple peers are identified per Owly's discovery result.
- `DeviceLost`: Quick Share after 15 s unseen; AirDrop when Owly reports the peer gone, or after 30 s.
- List order is stable while displayed (first-seen order), as in Android's sheet.

## 9. Transfer engine & storage
```
Outgoing: Connecting → AwaitingAcceptance{pin?} → Transferring{done,total} → Completed | Rejected | Cancelled{by} | Failed{reason}
Incoming: AwaitingAcceptance{pin?} → Transferring → Completed | Rejected | Cancelled | Failed
```
- **Destination:** `download_dir`, falling back to the XDG download dir, then `~/Downloads`.
- **Staging:** write into a per-transfer staging dir on the same filesystem, fsync, atomic rename on completion. Delete staging on failure or cancel.
- **Never overwrite.** Use `name (1).ext`, and so on.
- **Names:** basename only. Sanitize rather than refuse: keep the part after the last `/` or `\`, drop control chars (incl. `\0`), map empty, `.` and `..` to `file`, cut to 248 bytes keeping the extension (room for ` (N)`). Quick Share is saved flat. AirDrop folders are recreated only inside a per-transfer top folder after validation.
- **Before accept:** free space ≥ announced total + 64 MiB, otherwise `NOT_ENOUGH_SPACE` (Quick Share) or decline (AirDrop).
- **During transfer:** bytes per file ≤ announced size. AirDrop decompressed output is capped at `airdrop.max_upload_bytes`.
- **Concurrency:** many transfers in parallel; ≤ 1 pending prompt per peer and ≤ 3 globally; more are rejected as busy. Apple transfers also respect netd's radio arbitration (§13.2).
- **Text/URL:** stored in `Items`. The daemon never touches the clipboard.

## 10. D-Bus API (session bus, zbus)
Bus name `org.ldrop.Ldrop1`, root `/org/ldrop/Ldrop1`. D-Bus activation starts `ldropd`. Introspection XML lives in `packaging/dbus/interfaces/` (generated by `scripts/gen-dbus-xml.sh`).

The root also implements `org.freedesktop.DBus.ObjectManager`, so clients see transfer objects appear and disappear through `InterfacesAdded`/`InterfacesRemoved`. Finished transfers stay exported as Transfer objects until they drop out of the history.

**`org.ldrop.Ldrop1.Manager`**
| Kind | Member | Notes |
|---|---|---|
| prop | `DeviceName` `s` rw | |
| prop | `Visibility` `s` | `hidden` \| `everyone` |
| prop | `VisibilityExpiresAt` `t` | unix µs, 0 when hidden |
| prop | `DownloadDir` `s` rw | |
| prop | `Capabilities` `a{sv}` | one `a{sv}` per backend id (`lan`, `bt`, `wfd`, `hotspot`, `awdl-shared`, `awdl-exclusive`, `awdl-activemon`, `nan`) with `status` (`supported`\|`degraded`\|`unsupported`) and `reason`; plus `report` `s`, the full doctor report as JSON (for `ldrop doctor` and the GUI hardware page). Re-probed when read and older than 3 s, and every 30 s |
| method | `SetVisibility(s mode, u timeout_secs)` | 0 = default 600; `contacts`/`your-devices` → `NotSupported`. `Visibility` and `VisibilityExpiresAt` change in one `PropertiesChanged` |
| method | `StartDiscovery() → o` | session object; ends on `Stop()` or when the caller leaves the bus |
| method | `Send(s device_id, as uris, a{sv} opts) → o` | `file://` URIs |
| method | `SendText(s device_id, s text, b is_url) → o` | |
| method | `CreateQrSession() → (o session, s url)` | §1.5; device appears via that session's `DeviceFound` |
| method | `Accept(o)`, `Reject(o)`, `Cancel(o)` | |
| method | `RegisterAgent(o)`, `UnregisterAgent(o)` | BlueZ-style; last registered wins |
| method | `ListTransfers() → ao` | active transfers + last 50 finished. History is metadata only (names, sizes, peer, result, time) in `$XDG_STATE_HOME/ldrop/history.jsonl` |
| method | `ClearHistory()` | |
| signal | `IncomingRequest(o transfer)` | |

**`org.ldrop.Ldrop1.Discovery`** (`/org/ldrop/Ldrop1/discovery/N`): `Stop()`, `DeviceFound(s id, a{sv} props)`, `DeviceLost(s id)`.

**`org.ldrop.Ldrop1.Transfer`** (`/org/ldrop/Ldrop1/transfer/N`): `Direction s`, `State s`, `Peer a{sv}`, `Pin s`, `Items aa{sv}` (name, mime, size, path, text), `BytesTotal t`, `BytesDone t`, `Error s`, `Deadline t` (consent deadline, so a UI can show a countdown), `CreatedAt t`, optional `Preview ay` + `PreviewMime s` (≤ 256 KiB; thumbnail sent by the peer, for example the AirDrop `/Ask` file icon, so prompts can show a preview like Android). `PropertiesChanged` is throttled to ≤ 10 Hz.

**`org.ldrop.Ldrop1.Agent`** (frontend/CLI): `RequestAccept(o transfer, a{sv} info) → b`. `info` holds `peer` `a{sv}`, `pin` `s` (Quick Share only), `items` `aa{sv}`, `bytes_total` `t`, `deadline` `t`. Must answer before `Deadline`. With no agent, the daemon uses `org.freedesktop.Notifications` with Accept/Decline actions. `Manager.Accept`/`Reject` answer at any time, from any client; the first answer wins.

**`org.ldrop.Ldrop1.Debug`** (only with `ldropd --dev-sim`): `InjectIncoming(s peer_name, s platform, as names, t size_each) → o` starts a simulated incoming request. The simulated devices "Sim Pixel" and "Sim iPhone" appear in discovery; an item whose name contains "decline" makes them decline. For frontend development and tests.

**`org.ldrop.Gui1`** at `/org/ldrop/Gui1`, owned by the running GUI (not by ldropd): `Share(as uris, s activation_token)`, `ShowMain(s activation_token)`. Used for single-instance forwarding, the Purpose plugin and file-manager entries (§11.4, §11.6).

Errors: `org.ldrop.Error.{NotSupported, NotFound, Busy, Unavailable, PermissionDenied, Failed}`.

## 11. Frontends

All frontends are thin. They talk to `ldropd` only through `ldrop-client`, and none contains protocol code. Whichever frontend is open registers as the **Agent**. With none open, prompts go to desktop notifications (§10).

### 11.1 `ldrop-client` (shared Rust crate)
- Typed zbus proxies for `org.ldrop.Ldrop1`.
- **Reactive stores:** devices, transfers, visibility and countdown, capabilities. Each is a stream of snapshots.
- **Agent trait:** frontends implement `request_accept(transfer) -> Decision`, and the crate handles D-Bus registration and deadline expiry.
- Used by the CLI, TUI and GUI. The GUI wraps the stores as cxx-qt `QAbstractListModel`/QObject types.

### 11.2 CLI (`ldrop`)
Talks only to `ldropd`. `--json` on every read command. Exit codes: 0 ok, 1 error, 2 usage, 3 declined, 4 cancelled, 5 backend unavailable.
```
ldrop doctor [--json] [--card-test]
ldrop status
ldrop visible [--for 10m]
ldrop hidden
ldrop devices [--watch] [--timeout 15s]
ldrop send [--to <name|id>] <file>...      # interactive picker on TTY when --to omitted
ldrop send [--to …] --text "…" | --url https://…
ldrop send --qr <file>...                  # Quick Share QR flow (§1.5)
ldrop receive                              # foreground agent: request + PIN, y/N, progress
ldrop transfers [--watch]
ldrop accept|reject|cancel <transfer-id>
ldrop clear-history
ldrop config get [key] | set <key> <value> | path   # edits the file; ldropd reloads it
```
There is no auto-accept flag, by design.

Additional commands:
```
ldrop tui                     # alias for ldrop-tui
ldrop integrate <dolphin|nautilus|nemo|caja|thunar|pcmanfm-qt|all> [--remove]   # per-user installs where no system drop-in exists
```

### 11.3 TUI (`ldrop-tui`)
**Goal:** full use from a terminal on tiling WMs and minimal setups. Simple and keyboard-first.

- **Stack:** ratatui + crossterm [K12], tokio, `ldrop-client`. Works in 80×24 and adapts to resizes.
- **Layout:**
  ```
  ┌ ldrop · thinkpad · Everyone 7:42 ─ QS:lan,bt  AirDrop:awdl-shared ┐
  │ [1] Devices  [2] Transfers  [3] Settings                           │
  │ ▸ Pixel 9 Pro        phone   Android  Quick Share                  │
  │   Anna's iPhone      phone   Apple    AirDrop                      │
  │   Searching…                                                       │
  └ ↑↓/jk move · Enter send · v visibility · ? help · q quit ──────────┘
  ```
- **Devices tab:** live discovery list (name, kind, platform, protocol). Discovery runs only while this tab is shown, like Android's share sheet. Enter opens a send dialog (`t` opens it in text mode; Ctrl-T switches Files/Text/Link) with:
  - path input with Tab completion and multiple paths;
  - text/URL mode;
  - after sending: the PIN (Quick Share), "Waiting for accept" (AirDrop) and progress.
- **`ldrop-tui send FILE…`** starts with files preselected, so the user only picks a device. This is meant for WM keybinds and file-manager "open in terminal" setups.
- **Incoming modal:** sender, platform, items and total size, PIN, a countdown from `Transfer.Deadline`. Keys: `a` accept, `d` decline. Shown over any tab, with a terminal bell. The answer keys are ignored for the first second, so typing into a field when a request pops up can't answer it. A waiting transfer can also be answered from the Transfers tab.
- **Transfers tab:** progress bars, speed, ETA, history. Keys:
  - `c` cancel;
  - `o` open with `xdg-open`;
  - `y` copy text/URL with `wl-copy`/`xclip` if present;
  - `x` clear history.
- **Settings tab:** visibility (`v`: Hidden ↔ Everyone 10 min), device name, download dir, Quick Share/AirDrop toggles, exclusive-radio opt-in. The opt-in shows a plain warning: "Wi-Fi disconnects while Apple devices can see this computer."
- **Keys:** arrows/hjkl, Tab or 1–3 to switch tabs, `?` help, Esc to close, `q` to quit. Mouse optional.
- **Theming:**
  - By default only the terminal's 16 ANSI colors plus bold/reverse, so it inherits the terminal theme (base16, pywal, …).
  - `NO_COLOR` is honored.
  - `[tui] colors = "custom"` enables the overrides.
  - ASCII box/symbol fallback when the locale isn't UTF-8.
- **WM recipes** (`docs/wm.md`):
  - a waybar custom module (`ldrop status --json` exposes text + a class `hidden|everyone|transferring`; click toggles visibility);
  - picker scripts for rofi/fuzzel/wofi/dmenu (`ldrop devices --json` → `ldrop send --to <id> FILE…`);
  - notification daemons that support actions (mako, dunst, swaync) for accept/decline;
  - the tray via `ldrop-gui --tray` in any SNI host (waybar tray).

### 11.4 GUI (`ldrop-gui`)
**Stack.**
- Rust + cxx-qt [R52], Qt 6 QtQuick + Kirigami [R51][R59], and KF6 KStatusNotifierItem, KNotifications, KI18n, KColorScheme.
- QML lives in `crates/ldrop-gui/qml/`. Rust exposes models (devices, transfers, history, capabilities, settings) and commands only.

**Process model.**
- `ldrop-gui` opens the main window. `ldrop-gui --share FILE…|URI…` opens the share sheet. `ldrop-gui --tray` runs in the background with the tray.
- **Single instance:** the first process owns `org.ldrop.Gui1`. Later launches forward `Share`/`ShowMain`, with an xdg-activation token if one is available [K13], then exit.
- Registers as Agent while running. Autostart in the tray is opt-in.

**Surfaces (Android Quick Share parity):**
1. **Share sheet.**
   - Header with an item preview (thumbnails for images, icons otherwise), count and total size.
   - A grid of nearby devices, each with a device-type icon and a text badge, "Android · Quick Share" or "Apple · AirDrop". No vendor logos.
   - "Looking for nearby devices…" state, with a hint: "iPhone, iPad or Mac? Set AirDrop to *Everyone for 10 minutes*."
   - A "Use QR code" button (§1.5). Selecting several devices is allowed.
   - Per-device states: PIN (Quick Share), Waiting, progress ring, Done, Declined, Failed with reason, Cancel.
2. **Incoming request.**
   - Shown first as a notification, like Android's heads-up card, via KNotification: "<sender> wants to share 3 files (12 MB)", PIN in the body, actions **Accept / Decline / Details**.
   - *Details* or the tray opens a dialog with the full item list, the preview (`Transfer.Preview`), the PIN, and a countdown from `Transfer.Deadline`.
   - On Wayland, windows are only raised with an activation token from the notification action or the launcher. Never steal focus [K13].
3. **Main window** (Kirigami ApplicationWindow):
   - **Visibility card:** Hidden / Everyone for 10 minutes, with a live countdown.
   - **"Send files…"** through the FileChooser portal [K7].
   - **Recent transfers:**
     - open a file (OpenURI portal);
     - show it in its folder (`org.freedesktop.FileManager1.ShowItems` [K13]);
     - copy text, open URLs;
     - clear history.
   - **Settings page:** device name, download folder, Quick Share/AirDrop toggles, the exclusive-radio opt-in (with the warning above), start in tray, autostart.
   - **Hardware page:** the `doctor` capabilities in plain language, with fix hints. Examples: "Your Wi-Fi card (Intel AX211) supports AirDrop only by disconnecting Wi-Fi during use." "Router channel 1 → AirDrop shared mode needs 6, 44 or 149."
4. **Tray** (StatusNotifierItem via KStatusNotifierItem [K8]).
   - Icon states: hidden, everyone (tooltip shows the countdown), transferring (progress overlay).
   - Menu: Visibility, Send files…, Open ldrop, Quit.
   - Left click opens the main window.
   - With no SNI host (GNOME without the AppIndicator extension), the app works through the window and notifications alone.
5. **Completion notification:** "Received 3 files from Pixel 9 Pro", with actions **Open** and **Show in folder**.

*Implementation note (M4, tray first).* The tray and the notifications are implemented in Rust directly over D-Bus instead of through KStatusNotifierItem/KNotification: `org.kde.StatusNotifierItem` + `com.canonical.dbusmenu` for the tray, `org.freedesktop.Notifications` for prompts and completion, and `org.kde.JobViewServerV2` for progress on Plasma (a progress notification elsewhere). The shell renders all of them natively, so the user sees the same thing, and the background part needs no Qt. Until the window exists, the menu offers Hidden / Visible to everyone, Open download folder and Quit; middle-click toggles visibility.

**Accessibility & i18n.**
- Full keyboard navigation.
- `Accessible.name` on every control (AT-SPI).
- Honor font scaling and high contrast.
- KI18n domain `ldrop`, with RTL mirroring through Kirigami.

### 11.5 Theming: Union + every desktop
**Goal:** on Plasma, look exactly like the user's chosen style (Breeze, Breeze Dark, or a Union-based style). On every other desktop or WM, follow its light/dark, accent, contrast, icons and fonts, with live updates where the toolkit allows.

1. **Never hardcode a QtQuick Controls style or palette when the platform provides one.** Apps that hardcode a style don't follow Union selection [R46].
2. **Style resolution at startup**, first match wins:
   1. `QT_QUICK_CONTROLS_STYLE` env or `[gui] style`.
   2. Plasma session (`XDG_CURRENT_DESKTOP` contains `KDE`), or `QT_QPA_PLATFORMTHEME` set (kde, qt6ct, gtk3, xdgdesktopportal, …): leave it to the platform theme. On Plasma 6.7+ that follows System Settings, including Union [R46].
   3. Otherwise, `org.kde.desktop` if qqc2-desktop-style is installed.
   4. Else `org.kde.breeze`, the approach KDE apps use across desktops [R50].
   5. Else `Fusion`.
3. **Color scheme off Plasma.**
   - Read the portal Settings key `org.freedesktop.appearance` / `color-scheme` and watch `SettingChanged` [K7].
   - Apply Breeze Light or Breeze Dark through KColorSchemeManager, live.
   - Qt's own portal theme maps this key [R54][R53], but the GTK3 platform theme ignores it [R55]. So ldrop keeps its own listener (pattern from [R50]).
   - **Skip the listener** when `QT_QPA_PLATFORMTHEME` is `qt6ct` or Kvantum is active, so it doesn't override the user's configuration (a known conflict [R50]).
4. **Accent.**
   - Read `org.freedesktop.appearance` / `accent-color` (an RGB triple) where it is offered (GNOME, KDE, COSMIC, elementary, Budgie [R56]).
   - Off Plasma, apply it to Highlight, links and focus rings. On Plasma the color scheme already carries the accent.
   - Desktops without the key (XFCE, Cinnamon [R56]) keep the scheme default.
5. **Contrast & motion.** `contrast` = high switches to high-contrast palette adjustments [K7]. Honor a reduced-motion portal key if present (*verify* [K7]); otherwise use Kirigami animation units.
6. **Icons.**
   - Use freedesktop icon names via `Kirigami.Icon` / `QIcon::fromTheme`, so the system icon theme applies.
   - App-specific icons ship in `hicolor`.
   - Fallback theme `breeze`, with a bundled minimal subset if breeze-icons is missing.
7. **Fonts & metrics.** Platform fonts only. Layout uses `Kirigami.Units`. No hardcoded pixels.
8. **Window decorations.**
   - Server-side where offered: Plasma, and wlroots compositors with xdg-decoration.
   - GNOME Wayland has none, so Qt draws client-side decorations. Use an Adwaita-style decoration plugin (e.g. QAdwaitaDecorations [K15]) when installed. S0.7 checks the options in the Qt version you target.
9. **Live switching.** Color scheme, accent and contrast change live. A QtQuick style change needs a restart (Union does too [R46]). The Settings page shows "Restart ldrop to apply style".

**Union notes.**
- Union is a CSS-driven style engine with Qt Quick and Qt Widgets outputs [R47].
- Tech preview in Plasma 6.7 (QtQuick apps after a restart; Widgets stay Breeze) [R46]. Extended in Plasma 6.8 (14 Oct 2026) [R48][R49]. Release note: [R57].
- ldrop is a QtQuick app, so it **gets Union automatically on Plasma** whenever the user selects it, provided rule 1 holds.
- Union has no shipped GTK output [R47], so rules 3–8 cover non-Qt desktops.

### 11.6 Desktop integrations
| Integration | Mechanism | Installed file(s) |
|---|---|---|
| Dolphin "Send with ldrop" | KIO service menu [K9] | `share/kio/servicemenus/org.ldrop.Ldrop.desktop` → `ldrop-gui --share %F` |
| Nautilus | nautilus-python `MenuProvider` extension [K11] | `share/nautilus-python/extensions/ldrop.py` (package `ldrop-nautilus`) |
| Nemo | Nemo action [K11] | `share/nemo/actions/ldrop.nemo_action` |
| Caja, PCManFM-Qt | file-manager actions `.desktop` (DES-EMA) [K11], support per file manager verified in S0.7 | `share/file-manager/actions/ldrop.desktop` |
| Thunar | custom action in per-user `~/.config/Thunar/uca.xml` [K11] (no system drop-in) | `ldrop integrate thunar` adds/removes it, with a backup |
| KDE share menu | Purpose plugin [K10]: a small C++ plugin calling `org.ldrop.Gui1.Share` | `integrations/purpose/` (CMake) → KF6 Purpose plugin dir |
| "Open with…" | `.desktop`, `Exec=ldrop-gui --share %U`, MimeType `application/octet-stream` + common types; never sets itself as default handler | `share/applications/org.ldrop.Ldrop.desktop` |
| Tray | KStatusNotifierItem / SNI [K8] | in-app |
| Autostart (opt-in) | XDG autostart `ldrop-gui --tray` | written to `~/.config/autostart/` only when enabled |
| Software centers | AppStream metainfo [K14] | `share/metainfo/org.ldrop.Ldrop.metainfo.xml` |

- Every entry passes local paths. `ldrop-gui --share` converts them to `file://` URIs and forwards them to the running instance.
- App ID `org.ldrop.Ldrop` is a **placeholder**; switch to a reverse-DNS domain the project controls before release. The D-Bus names change with it.

## 12. Configuration
`$XDG_CONFIG_HOME/ldrop/config.toml`. Unknown keys produce a warning. An invalid file keeps the last good config and reports an error; it never resets.
```toml
device_name = ""                 # empty = pretty hostname
download_dir = ""                # empty = XDG download dir
visibility_timeout_secs = 600

[quickshare]
enabled = true
tcp_port = 0                     # 0 = random; fixed eases firewalls
mediums = ["lan", "wfd", "hotspot", "bt"]
prompt_timeout_secs = 60
allow_wifi_switch = false        # Bluetooth transfers may move the Wi-Fi card to a phone's hotspot or host one

[airdrop]
enabled = true
link = "auto"                    # auto | awdl | nan
allow_exclusive_radio = false    # Intel etc.: Wi-Fi drops while Apple link is up
max_upload_bytes = 68719476736   # 64 GiB

[wifi]
interface = ""                   # empty = auto-detect
```
GUI/TUI keys (same file):
```toml
[gui]
style = ""              # empty = auto (§11.5); else a QtQuick Controls style name
start_in_tray = false
autostart = false       # writes/removes ~/.config/autostart entry

[tui]
colors = "terminal"     # terminal (16 ANSI colors) | custom
[tui.custom]            # only read when colors = "custom"
accent = "cyan"
```
Firewall needs, which docs and `doctor` must report (Owly probes this too [R30]):
- mDNS 5353/udp on the LAN;
- Quick Share `tcp_port`;
- **8771/tcp on `awdl0`** [R27].

## 13. Privileged helper `ldrop-netd` & packaging

### 13.1 Service
- System service `ldrop-netd.service`, D-Bus activated, bus name `org.ldrop.NetHelper1`.
- Runs as a dedicated user (sysusers.d) with only `CAP_NET_ADMIN CAP_NET_RAW`.
- Hardening: `NoNewPrivileges`, `ProtectSystem=strict`, `ProtectHome=yes`, `PrivateTmp`, `RestrictAddressFamilies=AF_UNIX AF_NETLINK AF_PACKET AF_INET AF_INET6`, `SystemCallFilter=@system-service`, `MemoryDenyWriteExecute`, `RuntimeDirectory=ldrop`.
- The S0.3 study confirms the capability set Owly's radio code needs; adjust if it needs more (for example to run hostapd for exclusive mode [R27]).

### 13.2 Radio arbitration
- Exactly one owner of the card at a time: `idle | awdl | wfd | hotspot | nan`.
- `awdl` in shared mode coexists with infra. Exclusive/active-monitor and `hotspot` without STA+AP support take the card.
- Conflicting requests queue in FIFO order. An active transfer is never pre-empted. A discovery-only session yields to a transfer.
- **Always restore Wi-Fi**: on stop, on caller disappearance (bus-name watch), on netd restart (dirty marker in `/run/ldrop/`), and on SIGTERM. Mirror owlyd's restore behavior [R27].

### 13.3 API (minimal)
- `Probe() → a{sv}`
- `AcquireAppleLink(s mode) → (o handle, s ifname)`
- `AcquireWifiDirect() → (o, a{sv})`
- `JoinNetwork(s ssid, s psk) → o`
- `StartHotspot() → (o, a{sv})`
- `Release(o)`

Callers never pass raw interface names. Every handle is tied to the caller's bus name.

polkit:
- `org.ldrop.netd.radio`: `allow_active=yes`.
- `org.ldrop.netd.exclusive-radio`: `auth_self_keep`.

### 13.4 Fallback if Owly crates can't be embedded
Run Owly's `owlyd` as the AWDL owner. netd then asks owlyd to release the card before `wfd`/`hotspot`. Add a `Conflicts=` between `ldrop-netd` AWDL mode and `owlyd.service` if both are installed (AUR `owly` exists [R27]).

### 13.5 Packaging
- Arch `PKGBUILD` (`packaging/arch/`), systemd units (system netd plus user `ldropd.service`), D-Bus service and policy files, polkit, sysusers.d.
- Ship Owly's `drivers.toml` (or depend on the Owly package for it).
- No Flatpak.
- Split packages:
  - `ldrop`: daemons, CLI, TUI.
  - `ldrop-gui`: GUI, tray, desktop file, metainfo, Dolphin service menu, Nemo action, file-manager actions.
  - `ldrop-kde`: Purpose plugin.
  - `ldrop-nautilus`: depends on `python-nautilus`.
- GUI dependencies: `qt6-declarative`, `kirigami`, `kstatusnotifieritem`, `knotifications`, `ki18n`, `kcolorscheme`. Optional: `qqc2-desktop-style`, `breeze-icons`, `kimageformats` (JPEG2000 previews).
- References: BlueZ, wpa_supplicant and NetworkManager D-Bus docs [K5].

## 14. Security requirements
- **Threat model:** while visible or discovering, any nearby device can send BLE, Wi-Fi frames, mDNS, TCP/TLS/HTTP and protocol messages. All of it is hostile. Owly SPEC takes the same stance [R29].
- `#![forbid(unsafe_code)]` in protocol crates. `unsafe` only in thin, documented syscall/FFI wrappers. Review any `unsafe` in pinned Owly revisions on each bump.
- Bound every length field before allocating. Cap counts: files, TLVs, plist depth, protobuf size, CPIO entries.
- One `cargo-fuzz` target per ldrop parser: Quick Share frames, UKEY2, SecureMessage, endpoint info, mDNS, BLE service data, QR payload. Seed from M0 fixtures. CI runs each ≥ 5 min; nightly ≥ 1 h. Run Owly's fuzz/conformance suites on each bump.
- **Consent:** hidden by default, Everyone mode expires, never auto-accept, no bytes written before Accept.
- **Filesystem:** no traversal; reject symlinks, hardlinks, device nodes and setuid/setgid bits; never overwrite; staging dirs.
- **Crypto:** check the UKEY2 commitment; strict sequence numbers; rustls only; identity keys 0600.
- **Anti-spam:** prompt limits (§9) plus a 30 s cooldown for a peer after a reject.
- **Privacy:** rotate the Quick Share endpoint ID and AirDrop service ID per visibility session; advertise only when needed; never log file contents; peer names only at debug level.
- Regression-test the published Quick Share vulnerability classes [K6].
- **Testing ethics:** only test against Apple devices you own. Crashes in iOS `sharingd` take down other Continuity features [R30].
- CI: `cargo audit`, `cargo deny`.

## 15. Reuse & licensing
Project license: **GPL-3.0-or-later**. Every ported file gets an SPDX header and an origin note. Add `CREDITS.md`. S0.5 writes `docs/research/licenses.md`.

| Source | License | Use |
|---|---|---|
| Owly (t4t5/omdrop-owl) [R27][R28] | **MIT** (LICENSE file, © 2026 T4T5) | Dependency / vendored fork for AWDL, radio and AirDrop. MIT code can go into a GPL-3.0 work; keep the MIT notice. |
| rquickshare `core_lib` [R42] | GPL-3.0 | Port into `ldrop-quickshare` |
| google/nearby [R44] | verify (believed Apache-2.0) | `.proto`s, medium/upgrade semantics. Apache-2.0 can be combined into GPL-3.0 |
| NearDrop [R43] | verify | Protocol documentation (cite, don't copy code without checking the license) |
| OpenDrop [R37] | GPL-3.0 | Reference only |
| OWL [R36] | verify | Reference only (superseded by Owly) |
| Qt 6 | LGPL-3.0 / GPL | GUI toolkit |
| KDE Frameworks 6 (Kirigami, KStatusNotifierItem, KNotifications, KI18n, KColorScheme, Purpose) | LGPL-2.1+ (verify each) | GUI + integrations |
| cxx-qt [R52] | MIT / Apache-2.0 | Rust↔Qt bridge |
| ratatui, crossterm [K12] | MIT | TUI |

All of the rows above can be combined into a GPL-3.0-or-later work.

## 16. Milestones
Each milestone ends with its acceptance checks passing.

**M0: Research & decisions (no product code).** Outputs go in `docs/research/`.
- **S0.1** ADR-001, the Apple link layer: does Pixel↔iPhone use AWDL or Wi-Fi Aware?
  - Method: passive captures with your own devices, using Wireshark AWDL/NAN dissectors [K4], plus the evidence in [R10][R13][R14].
  - Non-blocking for v1.
- **S0.2** Owly field test: build Owly, run `owly doctor` and `owly test-card` [R33] on target laptops (Intel AX2xx/BE200, MT7925, RTL8852xE). Then iPhone (iOS 26.x) and Mac send/receive. Record throughput, the model-string → icon mapping, and consent timing.
- **S0.3** Owly as a library:
  - Read the `owly-radio`/`owly-awdl`/`owly-airdrop` public APIs and the `owlyd` lifecycle.
  - Decide: embed (preferred, §4) or external owlyd (§13.4).
  - List the upstream patches ldrop needs: consent hook, progress, cancel, multi-file.
  - Output: ADR-002.
- **S0.4** Quick Share:
  - Verify §5 against current Pixel and Samsung, and the rquickshare encoder (endpoint-info bits, PIN).
  - Derive the BLE peer-to-peer advertising format and the bandwidth-upgrade sequence from google/nearby [R44].
  - Capture fixtures.
- **S0.5** License audit (§15).
- **S0.6** Hardware matrix: merge the Owly table [R27] with P2P, AP and NAN iftype probes.
- **S0.7** UI stack:
  - current cxx-qt version and Kirigami integration [R51][R52][R58][R59];
  - Union behavior for a third-party QtQuick app on Plasma 6.8 [R46];
  - KStatusNotifierItem behavior on Plasma, GNOME (AppIndicator extension) and waybar;
  - Purpose plugin API [K10];
  - file-manager action formats per file manager [K9][K11];
  - GNOME Wayland decorations [K15].
  - Output: `docs/research/ui-stack.md`.
- *Accept:* ADR-001 and ADR-002 written; fixtures committed; matrix filled; `ui-stack.md` written.

**M1: Skeleton.** Workspace, config, ldropd D-Bus skeleton, `ldrop-client`, CLI, real `ldrop doctor`, user unit.
- *Accept:* on the dev desktop, `doctor --json` reports BT ok and Wi-Fi absent.

**M2: Quick Share over LAN.** Receive, send, BLE wake, PIN, text/URL, keep-alive, cancel.
- *Accept:* Pixel and Samsung ↔ Linux pass with 1 file, 500 files, a 4 GB file, text and URL, and cancel from each side; PINs match.

**M3: TUI.** All features available so far; registers as Agent.
- *Accept:* send/receive with a Pixel entirely from `ldrop-tui` in an 80×24 terminal; incoming prompt with PIN; cancel; ratatui snapshot tests pass.

**M4: GUI core.** Share sheet, incoming notification + dialog, main window, settings, tray, single instance.
- *Accept:* the M2 scenarios done entirely from the GUI on Plasma 6.8; prompt appears without stealing focus; tray reflects state.

**M5: Quick Share peer-to-peer.** BLE discovery, RFCOMM, upgrade to `wfd`/`hotspot`, netd v1 with arbitration.
- *Accept:* transfer works with no shared network; Wi-Fi is restored afterwards, including after `kill -9` of either daemon.

**M6: Apple link in netd.** owly-radio + owly-awdl embedded (or owlyd per ADR-002); `AcquireAppleLink` for shared, exclusive and active-monitor modes.
- *Accept:* on an MT7925 laptop `awdl0` comes up while Wi-Fi stays associated; on an Intel AX2xx laptop exclusive mode works and Wi-Fi comes back; an iPhone peer shows up as reachable.

**M7: AirDrop receive** (iPhone/Mac → Linux) through `ldrop-airdrop` with the agent consent hook.
- *Accept:* Linux shows in the iPhone's AirDrop sheet (Everyone mode); photos, a ≥ 1 GB video, multiple files and a URL arrive; decline returns 403; cancel works; the GUI prompt shows the AirDrop preview and a countdown.

**M8: AirDrop send** (Linux → iPhone/Mac).
- *Accept:* the iPhone appears in `ldrop devices` within 10 s on shared-mode hardware; files and URLs arrive; a decline exits with 3.

**M9: Wi-Fi Aware** (only if ADR-001 calls for it, or as an Intel improvement).
- *Accept:* the M5/M6 checks pass over NAN on supporting hardware and kernel; hwsim tests pass.

**M10: Theming + integrations.** The §11.5 rules and §11.6 integrations.
- *Accept:* the full theme matrix (§17) passes; "Send with ldrop" works in Dolphin, Nautilus, Nemo, Caja, Thunar and PCManFM-Qt; the KDE Share menu lists ldrop; "Open with" works; the autostart toggle works.

**M11: UX parity.** Merged list, visibility timer, history, notification fallback, agent API, concurrent transfers, QR send (§1.5) in the CLI, TUI and GUI.
- *Accept:* every item in the §1 checklist passes, and QR send to a phone that isn't in Everyone mode works.

**M12: Hardening & packaging.** Fuzz CI, security review against §14, systemd/polkit/sysusers, split PKGBUILD (§13.5), `docs/frontend-api.md` for third-party frontends.
- *Accept:* `makepkg` builds; `cargo deny` is clean; fuzz targets run 1 h clean.

## 17. Testing & verification
- **Requirement tracing** (as Owly does [R32]): give §1 and §5 requirements IDs (`QS-*`, `UX-*`). Tests cite the IDs, and `cargo xtask coverage` fails CI on any uncovered MUST.
- **Unit:**
  - UKEY2 against the google/ukey2 vectors [K1];
  - HKDF salts and keys against the [R43] constants;
  - PIN against rquickshare;
  - protobuf round-trips;
  - endpoint-info and QR codecs.
- **Integration (no radio):**
  - Quick Share: two `ldropd` instances in separate network namespaces over veth, full send/receive/cancel.
  - AirDrop layers 3–7: reuse Owly's veth user-netns harness [R32] with ldrop's consent hook in the loop.
  - D-Bus contract tests on a private `dbus-daemon`.
- **Radio:** `mac80211_hwsim` in network namespaces for the netd arbitration and restore paths; Linux↔Linux AWDL if Owly supports hwsim (check in S0.3); NAN once hwsim supports it [R26].
- **Hardware-in-the-loop** (`tests/hil/checklist.md`):
  - Peers: Pixel 9/10, Samsung (recent), an Android 10+ phone, iPhone on iOS 18/26, iPad, Mac on macOS 26.
  - Directions: send and receive. Payloads: 1 photo, 500 files, 4 GB, text, URL.
  - Laptop Wi-Fi: MT7925, RTL8852xE, Intel AX201/AX211/BE200, Qualcomm WCN6855 (negative test).
  - Record throughput, discovery latency (target ≤ 5 s for Android on LAN, ≤ 10 s for Apple on shared-mode hardware) and Wi-Fi downtime in exclusive mode.
  - Submit Owly `test-card` reports upstream [R33].
- **Network-safety regression:** after stop, crash or `kill -9` of ldropd or netd, Wi-Fi reconnects, no leftover interfaces (`iw dev`), no leftover NetworkManager profiles.
- **TUI:** ratatui `TestBackend` golden-buffer snapshots for every screen at 80×24 and 120×40. Covers `NO_COLOR` and the non-UTF-8 ASCII fallback.
- **GUI logic:** Rust unit tests for the cxx-qt models. QML tests with `qmltestrunner` for the share sheet, prompt and settings.
- **Golden screenshots:** rendered offscreen (`QT_QPA_PLATFORM=offscreen`) for light, dark, high contrast and a custom accent, with tolerance diffing in CI.
- **Theme matrix** (manual checklist `tests/hil/theming.md`):
  - Plasma 6.8 with Breeze, Breeze Dark, custom accent, and the Union style selected;
  - GNOME (light, dark, accent);
  - XFCE, Cinnamon, MATE, LXQt, COSMIC, Budgie;
  - Sway and Hyprland, each plain, with qt6ct, and with Kvantum.
  - Check for each: style, color scheme, accent, icons, fonts, decorations, live switching, tray.
- **Accessibility:** run with Orca on GNOME and Plasma; every control is reachable by keyboard and has an accessible name.
- **Integrations:** a scripted check that each file-manager entry launches `ldrop-gui --share` with the correct paths, including spaces and Unicode.

## 18. Risks & open questions
- **Intel = exclusive mode only:** Wi-Fi drops while the Apple link is up, and it is slow on channel 6 [R27]. This is the biggest gap against "same as Android" on built-in Wi-Fi. Mitigations: NAN (§7) if viable; prefer shared-mode chips; document the behavior clearly.
- **Shared mode** needs the AP on channel 6, 44 or 149 [R27].
- **Link layer** is unresolved in public sources [R10][R13][R14][R15] (ADR-001).
- **Owly** is young (released Oct 2026 [R35]); its API may churn and it has open protocol questions (§6.4 [R31]). Pin revisions and contribute upstream.
- **Quick Share peer-to-peer upgrade** is undocumented outside google/nearby [R43][R44] (S0.4).
- **Protocol drift:** Apple and Google can change things at any time, and the feature "could disappear overnight" per [R10]. Samsung changes broke rquickshare before [R42].
- **Context:** Google built its interop without Apple [R1][R6]. EU DMA background: [R4][R15][R17][R18]. Not legal advice.
- **RTL8761B** advertising-set count is unverified (S0.6).
- **cxx-qt** is early-development with a changing API [R52]. Pin versions; keep the QML↔Rust boundary thin (models + commands only).
- **Union** is new, and its style handling changed recently [R46]. Never hardcode a style; re-test on each Plasma release.
- **On GNOME** the Qt GUI won't be pixel-identical to libadwaita apps, and has client-side decorations; the tray needs the AppIndicator extension. This is the accepted trade-off of the one-GUI decision.
- **Accent color** is missing on XFCE and Cinnamon [R56]; the scheme default is used there.
- **File-manager integration formats are fragmented.** Thunar has per-user entries only.
- **Host firewalls** (ufw, firewalld) commonly drop inbound LAN TCP, which silently breaks receiving while sending still works (seen on both M2 test machines). `doctor` should detect an active firewall and print the rule to add for the configured `tcp_port` (M12).

## 19. What this spec deliberately leaves to sources
- **AirDrop and AWDL wire details:** normative in Owly `docs/SPEC.md` [R29], with notes in [R30] and conformance in [R32]. ldrop doesn't duplicate them.
- **Quick Share peer-to-peer medium internals:** google/nearby [R44], to be written up in S0.4.

## 20. Sources

**Looked up while writing this spec (2026-10-25):**
| # | Source | Used for |
|---|---|---|
| R1 | Google Security Blog, "Android Quick Share Support for AirDrop" (Nov 2025): https://security.googleblog.com/2025/11/android-quick-share-support-for-airdrop-security.html | Rust, P2P, Everyone mode, third-party review |
| R2 | 9to5Google, launch: https://9to5google.com/2025/11/20/android-quick-share-airdrop-pixel-10/ | Pixel 10 launch, UX |
| R3 | Android Authority: https://www.androidauthority.com/quick-share-works-with-airdrop-3618023 | Everyone mode |
| R4 | Android Authority, EU angle: https://www.androidauthority.com/google-airdrop-apple-quick-share-eu-rules-3618395/ | DMA context |
| R5 | Privacy Guides, Pixel 9: https://www.privacyguides.org/news/2026/02/24/google-expands-quick-shares-airdrop-support-to-pixel-9-series/ | expansion |
| R6 | The Register: https://www.theregister.com/2025/11/21/google_links_androids_quick_share/ | built without Apple |
| R7 | Wikipedia, Quick Share: https://en.wikipedia.org/wiki/Quick_Share | timeline |
| R8 | Wikipedia, AirDrop: https://en.wikipedia.org/wiki/AirDrop | background |
| R9 | MacRumors forum, Pixel 9: https://forums.macrumors.com/threads/android-to-iphone-airdrop-transfers-now-supported-on-pixel-9.2477858/ | expansion |
| R10 | 9to5Google, EU ruling (+ iOS 18.5 reader report): https://9to5google.com/2025/11/21/googles-airdrop-support-for-pixel-10-likely-exists-because-of-the-eus-apple-ruling/ | link-layer evidence |
| R11 | Business Standard, expansion beyond Pixel 10: https://www.business-standard.com/technology/tech-news/google-expands-quick-share-airdrop-interoperability-beyond-pixel-10-series-126021800863_1.html | expansion (search result only) |
| R12 | BW Businessworld, Galaxy S26: https://www.businessworld.in/article/samsung-galaxy-s26-airdrop-quick-share-android-2026-598862 | Samsung (search result only; page returned 403) |
| R13 | Fatbobman Swift Weekly #113: https://fatbobman.com/en/weekly/issue-113/ | Wi-Fi Aware claim, Qualcomm |
| R14 | "How the EU Finally United Android and AirDrop": https://algustionesa.com/how-the-eu-finally-united-android-and-airdrop/ | Wi-Fi Aware claim |
| R15 | Ars Technica (repost), "The EU made Apple adopt new Wi-Fi standards…": https://tagteam.harvard.edu/hub_feeds/3415/feed_items/16944008 | Wi-Fi Aware claim |
| R16 | MacRumors, iOS 26 Wi-Fi features: https://www.macrumors.com/2025/06/21/ios-26-adding-two-new-wi-fi-features/ | iOS 26 Wi-Fi Aware |
| R17 | EU DMA factsheet (May 2026): https://digital-markets-act.ec.europa.eu/factsheet-how-dma-making-smartphones-better-interoperability-and-data-portability-case-studies-2026-05-11_en | Wi-Fi Aware iOS 26.0 worldwide |
| R18 | EU DMA, Apple specification proceedings: https://digital-markets-act.ec.europa.eu/businesses-portal/interoperability/apple-specification-proceedings-interoperability-connected-devices-and-request-based-process_en | DMA background |
| R19 | LWN, cfg80211/nl80211 NAN Data Path: https://lwn.net/Articles/1059533/ | kernel NAN |
| R20 | linux-wireless, NDP series (Feb 2026): https://ratatoskr.run/linux-wireless/2026/02/15792994/t | kernel NAN |
| R21 | linux-wireless, mac80211 NAN v2 (Mar 2026): https://ratatoskr.run/linux-wireless/2026/03/15793851/t | kernel NAN |
| R22 | linux-wireless, iwlwifi-mld NAN DATA pt 1 v2 (May 2026): https://ratatoskr.run/linux-wireless/2026/05/15794692/t | Intel NAN |
| R23 | linux-wireless, iwlwifi-mld NAN (May 2026): https://ratatoskr.run/linux-wireless/2026/05/15794675/t | Intel NAN |
| R24 | linux-wireless, wireless-next NAN v2 (May 2026): https://ratatoskr.run/linux-wireless/2026/05/15794590/t | kernel NAN |
| R25 | kbuild, `nan_mac80211_phase2`: https://ratatoskr.run/oe-kbuild-all/2026/03/14735731 | kernel NAN status |
| R26 | CVE-2026-23040 (hwsim NAN): https://osv.dev/vulnerability/CVE-2026-23040 | hwsim NAN exists |
| R27 | **Owly** repo + README: https://github.com/t4t5/omdrop-owl | crates, modes, hardware, packages, restore behavior |
| R28 | Owly LICENSE (MIT): https://github.com/t4t5/omdrop-owl/blob/main/LICENSE | licensing |
| R29 | Owly `docs/SPEC.md`: https://github.com/t4t5/omdrop-owl/blob/main/docs/SPEC.md | AirDrop protocol (normative) |
| R30 | Owly `docs/implementation.md`: https://github.com/t4t5/omdrop-owl/blob/main/docs/implementation.md | ports, flags, discovery quirks, 403, testing ethics |
| R31 | Owly `docs/open-questions.md`: https://github.com/t4t5/omdrop-owl/blob/main/docs/open-questions.md | open issues, iOS 26.7.1 |
| R32 | Owly `docs/conformance.md`: https://github.com/t4t5/omdrop-owl/blob/main/docs/conformance.md | test harness, requirement tracing |
| R33 | Owly `hardware/README.md`: https://github.com/t4t5/omdrop-owl/blob/main/hardware/README.md | test-card reports |
| R34 | omdrop-owl write-up (earlier state): https://www.besthub.dev/articles/omdrop-owl-brings-airdrop-to-linux-on-mediatek-wi-fi-while-keeping-wi-fi-active-0a17d658b4d8 | throughput, randomized addresses |
| R35 | iPhoneSoft (13 Oct 2026): https://iphonesoft.fr/2026/10/13/airdrop-fonctionne-linux-projet-open-source | release date, throughput |
| R36 | OWL README: https://github.com/seemoo-lab/owl/blob/master/README.md | approach B, limits |
| R37 | OpenDrop: https://github.com/seemoo-lab/opendrop | approach B, limits, Contacts-Only needs |
| R38 | OpenDrop `config.py`: https://github.com/seemoo-lab/opendrop/blob/master/opendrop/config.py | port 8771, flags, TLS defaults (reference) |
| R39 | Intel community, AX210 injection: https://community.intel.com/t5/Wireless/Problem-injecting-unicast-packets-using-pcap-library/m-p/1474152 | Intel background |
| R40 | FlyingCarpet: https://github.com/spieglt/FlyingCarpet | rejected alternative |
| R41 | Linux Magazine, LocalSend: https://www.linux-magazine.com/Issues/2025/291/LocalSend/(tagID)/642 | rejected alternative |
| R42 | rquickshare: https://github.com/Martichou/rquickshare | Quick Share Rust base, GPL-3.0, limits |
| R43 | NearDrop `PROTOCOL.md`: https://github.com/grishka/NearDrop/blob/master/PROTOCOL.md | Quick Share bytes, keys, frames, QR mode |
| R44 | google/nearby: https://github.com/google/nearby | protos, mediums, upgrade |
| R45 | safeguard.sh overview: https://safeguard.sh/resources/blog/wireless-file-sharing-airdrop-quick-share-wi-fi-direct-privacy | secondary background |
| R46 | KDE, "Union: Summer 2026 Update" (Arjen Hiemstra): https://planet.kde.org/arjen-hiemstra-2026-08-05-union-summer-2026-update/ | Union status, hardcoded-style caveat, restart behavior |
| R47 | OSNews, Union progress: https://www.osnews.com/story/144515/kde-makes-steady-progress-on-union-its-unified-theme-engine/ | CSS input, Qt outputs, GTK only potential |
| R48 | linuxcompatible.org, Plasma 6.8 Union + Qt Widgets: https://www.linuxcompatible.org/story/kde-plasma-68-union-theming-engine-now-supports-legacy-qt-widgets | 6.8 changes (less authoritative) |
| R49 | Wikipedia, KDE Plasma 6: https://en.wikipedia.org/wiki/KDE_Plasma_6 | 6.8 release date, Union improvements |
| R50 | KDE Discuss, "Making KDE apps that theme cross-desktop" (Sep–Oct 2026): https://discuss.kde.org/t/making-kde-apps-that-theme-cross-desktop/50230 | portal listener + Breeze schemes on GNOME, qt6ct conflict |
| R51 | KDE Developer, Rust: https://develop.kde.org/docs/getting-started/rust/ | cxx-qt + Kirigami path |
| R52 | KDAB/cxx-qt: https://github.com/KDAB/cxx-qt | Rust↔Qt bridge; maturity caveat |
| R53 | Qt docs, QStyleHints (6.12): https://doc.qt.io/qt-6.12/qstylehints.html | `colorScheme` API |
| R54 | Qt source, QXdgDesktopPortalTheme: https://contribute.qt-project.org/doc/d8/db1/qxdgdesktopportaltheme_8cpp_source.html | portal color-scheme mapping |
| R55 | Gist, Qt6 GTK3 theme ignores color-scheme: https://gist.github.com/davidar/ddbe25c7038d7b88e5fddd1272724d8e | GNOME gap (unverified report) |
| R56 | accent-color portal proposal (repost): https://lemmy.imagisphe.re/post/5904 | which desktops endorse the accent key |
| R57 | KDE on Mastodon, Union release note: https://floss.social/@kde/116523469386107065 | Union release (search result only) |
| R58 | cxx-qt-kirigami example: https://github.com/antroids/cxx-qt-kirigami | example project |
| R59 | KDE Developer, Rust + Kirigami app tutorial: https://develop.kde.org/docs/getting-started/rust/rust-app/ | project structure |

**From prior knowledge, not fetched. Verify:**
| # | Source | Used for |
|---|---|---|
| K1 | google/ukey2: https://github.com/google/ukey2 | UKEY2 spec, test vectors |
| K2 | Chromium Nearby Share source (PIN derivation; linked from [R43]) | PIN |
| K3 | Stute et al., "One Billion Apples' Secret Sauce: Recipe for the Apple Wireless Direct Link Ad hoc Protocol", ACM MobiCom 2018 | AWDL background |
| K4 | Wireshark AWDL and NAN dissectors | S0.1 captures |
| K5 | BlueZ `doc/` D-Bus API; wpa_supplicant D-Bus API (https://w1.fi/wpa_supplicant/devel/dbus.html); NetworkManager D-Bus API | medium/radio control |
| K6 | SafeBreach "QuickShell" Quick Share research (2024) | vuln classes to regression-test |
| K7 | xdg-desktop-portal Settings (`org.freedesktop.appearance`: color-scheme, accent-color, contrast), FileChooser, OpenURI portals | theming, file picking |
| K8 | StatusNotifierItem spec + KF6 KStatusNotifierItem | tray |
| K9 | KIO service menus (KF6) | Dolphin entry |
| K10 | KDE Purpose plugin API (KF6) | share menu |
| K11 | nautilus-python MenuProvider; Nemo actions; DES-EMA file-manager actions (Caja, PCManFM-Qt); Thunar `uca.xml` | file-manager entries |
| K12 | ratatui, crossterm | TUI |
| K13 | Wayland xdg-activation-v1; `org.freedesktop.FileManager1` | focus, "show in folder" |
| K14 | AppStream metainfo spec | software centers |
| K15 | QAdwaitaDecorations (Qt Wayland decoration plugin) | GNOME decorations |
