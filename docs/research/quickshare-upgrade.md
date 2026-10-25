# Quick Share peer-to-peer: Bluetooth and bandwidth upgrade

Output of spike S0.4 (SPEC §5.2, §5.4). Everything here was read from google/nearby [R44],
Nearby Connections, mostly `connections/implementation/`. File names below are relative to
that directory. NearDrop [R43] stops at the LAN medium and never worked out the switch-over.

Implemented in `crates/ldrop-quickshare` (`bluetooth.rs`, `link.rs`, `channel.rs`) and tested
in memory by `tests/session.rs`.

## 1. Bluetooth classic

### Device name

When it is visible over Bluetooth, a device renames its adapter to an encoded name
(`bluetooth_device_name.cc`). The name is absl web-safe base64 of:

| Bytes | Field |
|---|---|
| 1 | `version << 5 \| pcp`: version 1, PCP `P2P_POINT_TO_POINT` = 3. Quick Share writes `0x23` |
| 4 | endpoint ID (ASCII) |
| 3 | service ID hash `FC 9F 5E` |
| 1 | flags: bit 0 = WebRTC connectable |
| 6 | reserved, zero |
| 1 | endpoint info length |
| ≤131 | endpoint info (same encoding as the mDNS `n` TXT record) |
| optional | UWB address length + address |

- absl writes web-safe base64 **without padding** and accepts it either way. Bluetooth names
  are at most 248 bytes, so the 131-byte info cap keeps the name inside the limit.
- Discoverers decode every name they see and drop anything that fails to parse or carries
  another service hash.

### RFCOMM service

- Service UUID: UUID v3 of the service ID, `NearbySharing` →
  `a82efa21-ae5c-3dde-9bbc-f16da7b16c5a` (`bluetooth_classic.cc`, `GenerateUuidFromString`).
- No pairing and no authorization: Nearby connects insecure RFCOMM.
- The bytes on the socket are the same as on TCP: 4-byte big-endian length + `OfflineFrame`.

### Making Android look for us

Android Nearby **pauses Bluetooth classic discovery** when its BLE supports extended
advertising (`p2p_cluster_pcp_handler.cc`, `StartBluetoothDiscoveryWithPause`). It resumes
only after its BLE scan finds a "legacy device" (`BleLegacyDeviceDiscoveredHandler`).

A legacy device announces itself with a plain legacy advertisement
(`mediums/ble.cc`, `CreateAdvertisingDataForLegacyDevice`):

- service data for UUID `0xFEF3` (Copresence, `ble_utils.cc`);
- value `kDummyAdvertisementValue`, the ASCII string `QCAAABACAAADAAAAAAAAAAA`;
- connectable, medium TX power.

Nearby itself only sends it from platforms *without* extended advertising
(`StartLegacyAdvertising` returns early otherwise). ChromeOS and Linux run Nearby that way:
Bluetooth classic discoverable, RFCOMM server, legacy advertisement. ldrop copies that.

## 2. BLE advertisements (peer-to-peer discovery)

Android advertises over BLE while visible. ldrop parses these to find phones without a shared
LAN. Two layers (`mediums/ble/ble_advertisement.cc`, then `ble_advertisement.cc`):

**Medium layer**, the Copresence service data:

| Bytes | Field |
|---|---|
| 1 | `version << 5 \| socket_version << 2 \| fast << 1 \| second_profile`; versions 1–2 |
| regular: 3 + 4 | service ID hash, then data length (u32 BE) |
| fast: 1 | data length (u8) |
| n | connections-layer advertisement |
| 2 | device token |
| optional | extra-fields mask |

**Connections layer** (the data above):

| Bytes | Field |
|---|---|
| 1 | `version << 5 \| pcp` (version 1, PCP 3) |
| regular only: 3 | service ID hash |
| 4 | endpoint ID |
| 1 + n | endpoint info: fast ≤17 bytes (no name), regular ≤131 |
| regular only: 6 | Bluetooth MAC address |
| optional | UWB address length + address, extra fields |

A headers-only advertisement points to GATT slots that hold the full advertisement. ldrop
does not read GATT yet; it relies on the phone also being discoverable over Bluetooth classic.

## 3. Mediums in the connection request

- `ConnectionRequestFrame.mediums` lists what the client can upgrade to.
- `medium_metadata` adds `supports_5_ghz`, the client's LAN `ip_address` (4 bytes) and
  `MediumRole` flags (`support_wifi_hotspot_client`, `support_wifi_direct_group_client`, …).
- Medium numbers are the same in `ConnectionRequestFrame.Medium` and `UpgradePathInfo.Medium`:
  1 = MDNS, 2 = Bluetooth, 3 = Wi-Fi hotspot, 4 = BLE, 5 = Wi-Fi LAN, 6 = Wi-Fi Aware,
  7 = NFC, 8 = Wi-Fi Direct, 9 = WebRTC, …

## 4. Bandwidth upgrade (BWU)

The **host** is the side that offers the new medium; in Nearby that is the server (the
receiver in Quick Share). The **joiner** connects to it. Files: `bwu_manager.cc`,
`base_bwu_handler.cc`, `mediums/*_bwu_handler.cc`, `endpoint_channel_manager.cc`.

### Choosing a medium

- `ChooseBestUpgradeMedium` takes the first medium in the host's preference list that the
  client listed and that is available.
- Never the medium already in use. Never Wi-Fi hotspot while Wi-Fi LAN is in use.
- Android triggers the upgrade after the transfer is accepted. ldrop does it from 1 MB up;
  smaller transfers stay on the medium they started on.

### Sequence

All frames are `OfflineFrame` V1 `BANDWIDTH_UPGRADE_NEGOTIATION` unless noted.

1. **Host → C0, encrypted:** `UPGRADE_PATH_AVAILABLE` with `UpgradePathInfo`:
   - Wi-Fi LAN: `wifi_lan_socket { ip_address (4 or 16 bytes), wifi_port }`;
   - hotspot: `wifi_hotspot_credentials { ssid, password, port, gateway, frequency }`;
   - Wi-Fi Direct: `wifi_direct_credentials { ssid, password, port, frequency, gateway }`;
   - `supports_disabling_encryption`, `supports_client_introduction_ack`.
2. **Joiner** connects the new channel C1 and writes, **in plaintext on C1**,
   `CLIENT_INTRODUCTION { endpoint_id = its own ID, supports_disabling_encryption }`.
3. **Host** checks the ID against the peer of C0. If it set `supports_client_introduction_ack`,
   it writes a plaintext `CLIENT_INTRODUCTION_ACK` on C1; the joiner waits for it.
4. **Both** write `LAST_WRITE_TO_PRIOR_CHANNEL` on C0 (encrypted) and hold further data.
5. On reading the peer's `LAST_WRITE_TO_PRIOR_CHANNEL`, each side writes
   `SAFE_TO_CLOSE_PRIOR_CHANNEL` on C0. If its own C1 is not ready yet, it remembers and
   answers once it is.
6. On reading `SAFE_TO_CLOSE_PRIOR_CHANNEL`, each side writes a plaintext `DISCONNECTION` on
   C0, reads (and drops) frames until C0 ends or 3 s pass, closes C0, and continues on C1.
7. Sequence numbers continue: C1 uses the same UKEY2 keys and the same per-direction counters.

**Failure:** if the joiner cannot connect, it writes `UPGRADE_FAILURE { medium }` on C0. The
host tears down that medium and offers the next one; when none is left, both stay on C0.

### Encryption on the new channel

- Joiner: `CLIENT_INTRODUCTION.supports_disabling_encryption` =
  `path.supports_disabling_encryption && medium ∈ {hotspot, Wi-Fi Direct}`.
- Host: sets `supports_disabling_encryption` in the path only for hotspot and Wi-Fi Direct,
  and encrypts C1 unless the introduction asked to disable it *and* the medium allows it.
- These media are private WPA2 links set up for this one transfer, so SecureMessage on top is
  dropped for speed. Wi-Fi LAN and Bluetooth stay encrypted.

### Chunk sizes

- Bluetooth: 64 KiB payload chunks. Wi-Fi mediums: 512 KiB.
- Nearby's default packet size is 64 KiB and it reads frames up to 1 MiB; ldrop's frame limit
  (5 MiB) covers both.

## 5. Hosting and joining on Linux

- **Wi-Fi LAN:** host a TCP listener on the LAN IPv4 and send that address. Only offered when
  the client's `ip_address` shows it is on a LAN at all.
- **Hotspot (host):** NetworkManager AP connection with `ipv4.method = shared`, a random
  SSID/WPA2 password, then a TCP listener on the AP address (the gateway).
- **Hotspot / Wi-Fi Direct (join):** connect as a plain WPA2 station with a temporary
  NetworkManager connection, then TCP to `gateway:port`. Android's Wi-Fi Direct group owner
  accepts legacy stations, so no P2P stack is needed for joining.
- **Wi-Fi Direct (host):** needs a P2P group owner (wpa_supplicant P2P), which NetworkManager
  does not expose without root. Skipped for now.
- Cards with a single channel and no AP+STA combination drop the infrastructure connection
  when hosting or joining. That is why switching Wi-Fi is opt-in (`quickshare.allow_wifi_switch`).
