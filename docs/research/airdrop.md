# AirDrop: how ldrop does it

Design notes for SPEC §6. Implemented in `crates/ldropd/src/airdrop/` (`link.rs`, `receive.rs`,
`send.rs`), `crates/ldrop-wifi/src/owly.rs`, `crates/ldrop-airdrop/` and
`crates/ldrop-ble/src/monitor.rs`, and tested end to end on a veth pair by
`crates/ldropd/tests/airdrop.rs`.

## ADR-002: Owly's owlyd owns the radio

**Status:** accepted (October 2026).

**Context.** SPEC Revision 3 planned `ldrop-netd`, a system service of ldrop's own that
embeds `owly-radio` and `owly-awdl` and arbitrates the card between AWDL, Wi-Fi Direct and
hotspot. Owly v0.4 (2026-10-21) then shipped `owlyd`: a privileged radio service that leases the
card over a polkit-guarded system D-Bus API (`io.github.t4t5.Owly.Radio1`), keeps a
crash-recovery journal, restores Wi-Fi, and is packaged for Arch (AUR `owly`), Debian/Ubuntu,
Fedora and NixOS.

**Decision.** ldrop uses owlyd as an external system dependency for the Apple link and has no
privileged helper for AirDrop. ldropd is a client of `Radio1`:

- `Acquire(0)` when the first user needs the link, `Release` 10 s after the last is done. A
  lease ends when ldropd leaves the bus, so a crash gives the card back.
- `Peers` lists AWDL peers to knock on when sending; `Probe` feeds `ldrop doctor`.
- The protocol is `owly-airdrop` 0.1.0, vendored in `third_party/owly-airdrop` with small
  patches (see its README) until upstream has them.

**Consequences.**

- Less privileged code in ldrop, and Owly's card support and Wi-Fi restore come for free.
- Users install one more package, and over ssh need a polkit rule for
  `io.github.t4t5.Owly.acquire` (owlyd allows the active local session only).
- Arbitration between the Apple link and Quick Share's Wi-Fi switching moves into ldropd (a
  radio lock, `crates/ldropd/src/radio.rs`). Wi-Fi Direct, which needs root, would bring netd
  back (SPEC §13).

## Link on demand

Receivers don't keep AWDL up. While visible, ldropd watches passively for the AirDrop BLE
advert (BlueZ `AdvertisementMonitorManager1`, manufacturer data `4c 00 05`, RSSI above
`airdrop.wake_rssi`). That advert means a nearby Apple device has its share sheet open, which
is the only time anyone can send. The link, receiver and mDNS announcement come up then and go
away again when nothing has happened for a while. This mirrors Apple's own receivers. On
exclusive-mode cards it means Wi-Fi drops only while someone nearby actually shares.

## Sending

Following owly-cli's sender (`crates/owly-cli/src/airdrop.rs` in Owly v0.4.1):

1. Advertise `Advert::anonymous(random tag)` under Apple's company ID, connectable, every
   100 ms, for the whole discovery session and every send. Apple devices in *Everyone* mode
   wake their AirDrop when they see it; an iPhone opened its port 15 s after the advert in one
   of Owly's runs.
2. Browse `_airdrop._tcp` on the link, and send `/Discover` to `[fe80::EUI-64(mac)]:8770` for
   each peer owlyd lists, every 500 ms with a 3 s timeout. iPhone mDNS doesn't reach a Linux
   `awdl0` (Owly DISC-5), so the knock is what finds iPhones.
3. A receiver is listed only if it answers with its name (DISC-1). Knocked-on receivers are
   assumed to take DVZip (UP-1b).
4. owly-awdl pings each peer every 3 s (OWLY-2), so ldropd doesn't.

## Testing

Only against Apple devices you own, and only with well-formed requests. A crash in Apple's
`sharingd` takes down other Continuity features, so nothing is ever fuzzed against real
devices. Neighbors' devices that show up in discovery are observed, never sent to.
