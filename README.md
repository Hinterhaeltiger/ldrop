# ldrop

Quick Share and AirDrop for Linux: send to and receive from Android phones and Apple devices,
peer to peer. The design is in [SPEC.md](SPEC.md).

**Status: milestone M3 (terminal interface) plus the tray icon of M4.** Sending to and
receiving from Quick Share devices on the same network works from the CLI, the TUI and the
tray: discovery over mDNS with the BLE wake, PIN, files, text and links, decline, cancel from
either side. Incoming requests show up as desktop notifications with Accept/Decline, and
progress as a notification. Without a shared network, Quick Share also runs over Bluetooth
and moves to Wi-Fi once the transfer starts (the same network, or a hotspot when allowed).
AirDrop, hosting Wi-Fi Direct groups and the GUI window are later milestones. To try the frontends without phones, run the daemon with simulated peers
(`--dev-sim`).

## Layout

| Path | What |
|---|---|
| `crates/ldrop-core` | Shared types, config, history, capability rules, D-Bus wire types |
| `crates/ldrop-wifi` | nl80211 Wi-Fi capability probe, NetworkManager hotspot sessions |
| `crates/ldrop-ble` | BlueZ capability probe, BLE advertisements |
| `crates/ldrop-bt` | Bluetooth Classic through BlueZ: RFCOMM service, discovery, adapter visibility |
| `crates/ldrop-quickshare` | Quick Share protocol: UKEY2, SecureMessage, frames, send and receive sessions |
| `crates/ldrop-mdns` | mDNS publish and browse |
| `crates/ldropd` | User daemon, owns `org.ldrop.Ldrop1` on the session bus |
| `crates/ldrop-client` | Client library for frontends: proxies, discovery, transfers, Agent |
| `crates/ldrop-cli` | `ldrop` command |
| `crates/ldrop-tui` | `ldrop-tui` terminal interface |
| `crates/ldrop-gui` | `ldrop-gui` tray icon and notifications |
| `docs/` | Recipes for window managers ([docs/wm.md](docs/wm.md)) |
| `packaging/` | systemd user unit, D-Bus activation file, desktop entry, API introspection XML |

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
Quick Share port and mDNS (5353/udp). Transfers that start over Bluetooth move to the LAN on
the next port up. With a firewall, pin the port and open both, for example:

```sh
ldrop config set quickshare.tcp_port 47000                  # then restart ldropd
sudo ufw allow 47000:47001/tcp && sudo ufw allow 5353/udp    # or: firewall-cmd --add-port=…
```

Sending only needs outbound connections.

**Bluetooth.** When the phone and the computer share no network, Quick Share runs over
Bluetooth classic, with no pairing. It needs BlueZ and a powered adapter (ldrop never powers
it on). It is slow, about 100–200 KB/s, so it suits photos and documents more than videos.

- *Receiving:* while visible, ldrop renames the adapter to an encoded Quick Share name (the
  string other Bluetooth devices see looks like `I1RGNEv8n14…`), makes it discoverable, and
  sends a short BLE advertisement so Android phones start looking. Hiding puts the old name
  and visibility back; after a crash, the next start does.
- *Sending:* `ldrop devices` also lists phones that are visible over Bluetooth. ldrop uses the
  LAN when the device is there too, and Bluetooth otherwise.
- *Faster after the start:* once a transfer over 1 MB is accepted, the connection moves from
  Bluetooth to Wi-Fi if it can. If both are on the same network, that happens on its own. Moving
  to a phone's hotspot, or hosting one for the phone, takes the computer off its current Wi-Fi
  network for the duration of the transfer, so it is off by default:
  `ldrop config set quickshare.allow_wifi_switch true`. ldrop puts the old network back
  afterwards, and NetworkManager does so even if ldropd crashes. This needs NetworkManager,
  and dnsmasq for hosting.

To use only one of them: `ldrop config set quickshare.mediums lan` (or `bt`; the default is
`lan,wfd,hotspot,bt`), then restart ldropd.

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

## Tray and notifications

`ldrop-gui` puts an icon in the system tray (any StatusNotifierItem host: Plasma, GNOME with
the AppIndicator extension, Waybar's `tray`, …) and handles requests through desktop
notifications:

- **Tray menu:** the current state, *Hidden* / *Visible to everyone* (the daemon's default
  of 10 minutes), *Open download folder*, *Quit*. Middle-click toggles visibility. The icon and
  tooltip show the visibility countdown and transfer progress.
- **Incoming request:** a notification "Pixel 9 wants to share 2 files" with the PIN, the
  first file and the size, and the actions **Accept** and **Decline**. Dismissing it declines;
  no answer before the deadline declines too.
- **Progress:** a second notification with the percentage, size and speed, and **Cancel**.
  On Plasma it is a native job in the notification area instead.
- **Done:** "Received 2 files from Pixel 9" with **Open** and **Show in folder**.

```sh
target/debug/ldropd --dev-sim &
target/debug/ldrop-gui &
busctl --user call org.ldrop.Ldrop1 /org/ldrop/Ldrop1 org.ldrop.Ldrop1.Debug \
    InjectIncoming ssast "Pixel 9" android 2 photo.jpg notes.pdf 4000000
```

Only one `ldrop-gui` runs per session. The tray needs a notification server with actions
for the prompt (all desktops have one; on window managers mako, dunst or swaync); without
one, `ldrop-gui` leaves prompts to another frontend. To start it with the session, copy
`packaging/desktop/org.ldrop.Ldrop.desktop` to `~/.config/autostart/` (and to
`~/.local/share/applications/` so notifications show the app name and icon).

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
