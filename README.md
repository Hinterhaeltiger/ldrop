# ldrop

Quick Share and AirDrop for Linux: send to and receive from Android phones and Apple devices,
peer to peer. The design is in [SPEC.md](SPEC.md).

**Status: milestone M3 (terminal interface).** Sending to and receiving from Quick Share
devices on the same network works from the CLI and the TUI: discovery over mDNS with the BLE
wake, PIN, files, text and links, decline, cancel from either side. AirDrop, peer-to-peer
Wi-Fi and the GUI are later milestones. To try the frontends without phones, run the daemon
with simulated peers (`--dev-sim`).

## Layout

| Path | What |
|---|---|
| `crates/ldrop-core` | Shared types, config, history, capability rules, D-Bus wire types |
| `crates/ldrop-wifi` | nl80211 Wi-Fi capability probe |
| `crates/ldrop-ble` | BlueZ capability probe, BLE advertisements |
| `crates/ldrop-quickshare` | Quick Share protocol: UKEY2, SecureMessage, frames, send and receive sessions |
| `crates/ldrop-mdns` | mDNS publish and browse |
| `crates/ldropd` | User daemon, owns `org.ldrop.Ldrop1` on the session bus |
| `crates/ldrop-client` | Client library for frontends: proxies, discovery, transfers, Agent |
| `crates/ldrop-cli` | `ldrop` command |
| `crates/ldrop-tui` | `ldrop-tui` terminal interface |
| `docs/` | Recipes for window managers ([docs/wm.md](docs/wm.md)) |
| `packaging/` | systemd user unit, D-Bus activation file, API introspection XML |

## Build and try

Needs Rust 1.89+ and `protoc` (Arch: `protobuf`).

```sh
cargo build
target/debug/ldropd --dev-sim &      # simulated "Sim Pixel" and "Sim iPhone"
target/debug/ldrop-tui               # the terminal interface, or the commands below
target/debug/ldrop doctor            # what this machine's hardware supports
target/debug/ldrop devices
target/debug/ldrop send --to "Sim Pixel" README.md
target/debug/ldrop receive           # in another terminal, then:
busctl --user call org.ldrop.Ldrop1 /org/ldrop/Ldrop1 org.ldrop.Ldrop1.Debug \
    InjectIncoming ssast "Pixel 9" android 1 photo.jpg 2000000
```

Logging: `LDROP_LOG=debug`.

With a phone on the same network, run `target/debug/ldropd` without `--dev-sim`, then
`ldrop visible` + `ldrop receive` to receive, or `ldrop send --to "<phone name>" FILE…` to send.

**Firewall.** Phones connect to this computer to send, so receiving needs inbound TCP on the
Quick Share port and mDNS (5353/udp). With a firewall, pin the port and open it, for example:

```sh
ldrop config set quickshare.tcp_port 47000            # then restart ldropd
sudo ufw allow 47000/tcp && sudo ufw allow 5353/udp    # or: firewall-cmd --add-port=…
```

Sending only needs outbound connections.

## Terminal interface

`ldrop-tui` (also `ldrop tui`) is for window managers and terminals. It has three tabs:
**Devices** (nearby devices; Enter sends files, `t` sends text or a link), **Transfers**
(progress, speed, history) and **Settings** (visibility, device name, download folder,
Quick Share and AirDrop switches). Incoming requests pop up over any tab with the sender, the
files, the PIN and a countdown; `a` accepts, `d` declines.

```sh
ldrop-tui                     # browse, send, receive
ldrop-tui send photo.jpg      # files already picked, just choose the device
```

| Key | Does |
|---|---|
| `1` `2` `3`, Tab | switch tabs |
| ↑↓ or `j` `k` | move |
| Enter | send to the device / change the setting |
| `t` | send text or a link (Ctrl-T switches Files, Text, Link in the send dialog) |
| `v` | visible to everyone for 10 minutes / hidden |
| `a`, `d` | accept, decline an incoming request |
| `c` | cancel the selected transfer |
| `o` | open what was received (`xdg-open`) |
| `y` | copy received text, a link or file paths (`wl-copy` or `xclip`) |
| `x` | clear the history |
| `?`, Esc, `q` | help, close, quit |

In the send dialog, type a path (Tab completes it) and press Enter to add it; Enter on an
empty line sends. The TUI uses the terminal's own colors, honors `NO_COLOR`, and falls back
to ASCII when the locale isn't UTF-8. A custom accent color goes in the config:

```sh
ldrop config set tui.colors custom
ldrop config set tui.custom.accent magenta     # a color name or #rrggbb
```

Waybar, rofi/fuzzel pickers and notification setups: see [docs/wm.md](docs/wm.md).

## Tests

```sh
cargo test --workspace
```

The D-Bus tests in `crates/ldropd/tests/` start a private `dbus-daemon` and are skipped when it
is not installed. The TUI compares every screen against text snapshots in
`crates/ldrop-tui/snapshots/`; after an intended change to the layout, rewrite them with
`LDROP_UPDATE_SNAPSHOTS=1 cargo test -p ldrop-tui` and review the diff.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
