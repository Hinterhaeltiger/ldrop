# ldrop

Quick Share and AirDrop for Linux: send to and receive from Android phones and Apple devices,
peer to peer. The design is in [SPEC.md](SPEC.md).

## What works

- **Quick Share** (Android) in both directions: files, text and links, with PIN, decline and
  cancel from either side.
  - Over the local network (mDNS discovery), or over Bluetooth when there is no shared network.
  - Bluetooth transfers move to Wi-Fi once they start: the same network, or a hotspot when allowed.
- **Frontends:**
  - `ldrop` command;
  - `ldrop-tui` terminal interface;
  - `ldrop-gui` for the tray and desktop notifications (Accept/Decline, progress);
  - a share sheet window, opened from file managers ("Send with ldrop") and the KDE Share menu;
  - a Plasma system-tray applet.
- **Not yet:** AirDrop, Wi-Fi Direct hosting, and the main GUI window.

## Requirements

- **To build:** Rust 1.89+, `protoc`, a C++ toolchain, and Qt 6 (for `ldrop-gui`).
  - Arch: `rust protobuf base-devel qt6-declarative`.
  - Debian/Ubuntu: `cargo protobuf-compiler build-essential qt6-declarative-dev
    qt6-base-dev`.
  - The KDE Share menu plugin also needs CMake and KDE Frameworks' Purpose: Arch `cmake
    purpose`; Debian/Ubuntu `cmake libkf6purpose-dev`.
- **To run:**
  - a D-Bus session;
  - BlueZ with a powered adapter, for Bluetooth;
  - NetworkManager, for Wi-Fi switching (plus dnsmasq for hosting a hotspot);
  - a notification server, for the tray prompts;
  - Kirigami, for the share sheet (Arch `kirigami`, Debian/Ubuntu `qml6-module-org-kde-kirigami`).
  - Optional: Plasma 6, for the tray applet.

## Build

```sh
git clone https://github.com/Hinterhaeltiger/ldrop
cd ldrop
cargo build --release
```

This produces `ldropd` (daemon), `ldrop` (CLI), `ldrop-tui` and `ldrop-gui` in
`target/release/`.

## Install

### System-wide

```sh
sudo install -Dm755 target/release/{ldropd,ldrop,ldrop-tui,ldrop-gui} -t /usr/bin/
sudo install -Dm644 packaging/systemd/user/ldropd.service -t /usr/lib/systemd/user/
sudo install -Dm644 packaging/dbus/services/org.ldrop.Ldrop1.service -t /usr/share/dbus-1/services/
sudo install -Dm644 packaging/desktop/org.ldrop.Ldrop.desktop -t /usr/share/applications/
```

### For your user only (no root)

The unit files point at `/usr/bin`, so rewrite the path while copying them:

```sh
install -Dm755 target/release/{ldropd,ldrop,ldrop-tui,ldrop-gui} -t ~/.local/bin/
mkdir -p ~/.config/systemd/user ~/.local/share/dbus-1/services ~/.local/share/applications
sed "s|/usr/bin|$HOME/.local/bin|" packaging/systemd/user/ldropd.service > ~/.config/systemd/user/ldropd.service
sed "s|/usr/bin|$HOME/.local/bin|" packaging/dbus/services/org.ldrop.Ldrop1.service \
    > ~/.local/share/dbus-1/services/org.ldrop.Ldrop1.service
sed "s|^Exec=ldrop-gui|Exec=$HOME/.local/bin/ldrop-gui|" packaging/desktop/org.ldrop.Ldrop.desktop \
    > ~/.local/share/applications/org.ldrop.Ldrop.desktop
systemctl --user daemon-reload
```

`~/.local/bin` must be on your `PATH` for the `ldrop` commands.

## Set up

1. **Start the daemon.** Do it now and with every login:

   ```sh
   systemctl --user enable --now ldropd
   ```

   Any frontend also starts it through D-Bus on demand.
2. **Open the firewall** if you run one (ufw, firewalld):
   - Phones connect to this computer to send, so receiving needs inbound TCP on the Quick
     Share port, plus mDNS (5353/udp).
   - Transfers that start over Bluetooth move to the LAN on the next port up.
   - Pin the port and open both:

   ```sh
   ldrop config set quickshare.tcp_port 47000                  # then: systemctl --user restart ldropd
   sudo ufw allow 47000:47001/tcp && sudo ufw allow 5353/udp    # or: firewall-cmd --add-port=…
   ```

   Sending only needs outbound connections.
3. **Tray and notifications.** Start `ldrop-gui`. To start it with every session, add it to
   autostart:

   ```sh
   mkdir -p ~/.config/autostart
   cp ~/.local/share/applications/org.ldrop.Ldrop.desktop ~/.config/autostart/   # or /usr/share/applications/…
   ```
4. **Plasma only: the tray applet.** Install it for your user:

   ```sh
   kpackagetool6 -t Plasma/Applet -i packaging/plasma/org.ldrop.visibility   # -u to update, -r org.ldrop.visibility to remove
   systemctl --user restart plasma-plasmashell                                # or log in again
   ```
5. **"Send with ldrop" in your file manager.** Copy the entry for yours (for all users, use
   `/usr/share/…` instead of `~/.local/share/…`):

   ```sh
   # Dolphin (per-user service menus must be executable)
   install -Dm755 integrations/dolphin/org.ldrop.Ldrop.share.desktop -t ~/.local/share/kio/servicemenus/
   # Nautilus (needs nautilus-python), then: nautilus -q
   install -Dm644 integrations/nautilus/ldrop.py -t ~/.local/share/nautilus-python/extensions/
   # Nemo
   install -Dm644 integrations/nemo/ldrop.nemo_action -t ~/.local/share/nemo/actions/
   # Caja (with caja-actions) and PCManFM-Qt
   install -Dm644 integrations/file-manager-actions/ldrop.desktop -t ~/.local/share/file-manager/actions/
   # Thunar keeps custom actions in ~/.config/Thunar/uca.xml; this adds ours (--remove takes it out)
   ldrop integrate thunar
   ```
6. **KDE Share menu.** This puts ldrop in the Share menu of Dolphin, Gwenview, Spectacle and
   other KDE apps. It is a small Qt plugin, installed next to KDE's own:

   ```sh
   cmake -S integrations/purpose -B build/purpose -DCMAKE_INSTALL_PREFIX=/usr -DCMAKE_BUILD_TYPE=Release
   cmake --build build/purpose && sudo cmake --install build/purpose
   ```
7. **Check the hardware.** `ldrop doctor` shows which transports this machine supports, and why
   others are missing.

## Use

```sh
ldrop visible                         # visible to everyone nearby for 10 minutes
ldrop receive                         # accept incoming shares in this terminal (or use the tray)
ldrop devices                         # who is nearby
ldrop send --to "Pixel 9" photo.jpg   # send files; or --text "hi", --url https://…
ldrop-tui                             # all of it in a terminal interface
```

Received files go to your XDG download folder (`ldrop config set download_dir …` to change
it). Logging: `LDROP_LOG=debug`, or for the service, `journalctl --user -u ldropd`.

**Bluetooth.** When the phone and the computer share no network, Quick Share runs over
Bluetooth classic, with no pairing. It needs BlueZ and a powered adapter (ldrop never powers
it on). It is slow, about 100–200 KB/s, so it suits photos and documents more than videos.

- *Receiving:* while visible, ldrop:
  - renames the adapter to an encoded Quick Share name (other Bluetooth devices see a string
    like `I1RGNEv8n14…`);
  - makes the adapter discoverable;
  - sends a short BLE advertisement so Android phones start looking.

  Hiding puts the old name and visibility back. After a crash, the next start does.
- *Sending:* `ldrop devices` also lists phones that are visible over Bluetooth. ldrop uses the
  LAN when the device is there too, and Bluetooth otherwise.
- *Faster after the start:* once a transfer over 1 MB is accepted, the connection moves from
  Bluetooth to Wi-Fi if it can.
  - If both are on the same network, that happens on its own.
  - Moving to a phone's hotspot, or hosting one for the phone, takes the computer off its
    current Wi-Fi network for the duration of the transfer. So it is off by default; turn it
    on with `ldrop config set quickshare.allow_wifi_switch true`.
  - ldrop puts the old network back afterwards, and NetworkManager does so even if ldropd
    crashes.

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

**Plasma.** The tray applet (Set up, step 4) takes the place of the tray icon. Click it for a
popup with *Hidden* / *Everyone for 10 minutes*, the time left, and the download folder;
right-click has the same. It shows up while ldropd runs. With the applet installed,
`ldrop-gui` shows no tray icon of its own on Plasma and only handles notifications;
`ldrop-gui --tray-icon` brings the icon back.

Only one `ldrop-gui` runs per session. The prompts need a notification server with actions:
all desktops have one, and on window managers mako, dunst or swaync do. Without one,
`ldrop-gui` leaves prompts to another frontend.

## Share sheet

`ldrop-gui --share FILE…` opens a window for sending, which is what the file manager entries and
the KDE Share menu call:

- Nearby devices appear as they are found, with their platform and protocol.
- **Send** starts the transfer. The row then shows the PIN to compare with the phone, waits for
  them to accept, and shows the progress, with **Cancel**.
- At the end, the row shows *Sent*, *Declined* or the reason it failed (**Retry** sends again).
- You can send to several devices from one sheet. Closing the window does not stop transfers
  that already started; `ldrop transfers` or the tray still show them.

The sheet uses the desktop's Qt style: Breeze (or Union) on Plasma, and qqc2-desktop-style
elsewhere when no platform theme is set.

## Development

Try the frontends without phones: `--dev-sim` adds the simulated devices "Sim Pixel" and
"Sim iPhone", and the Debug interface fakes incoming shares.

```sh
cargo build
target/debug/ldropd --dev-sim &
target/debug/ldrop-tui                 # or ldrop-gui, or the ldrop commands
target/debug/ldrop send --to "Sim Pixel" README.md
busctl --user call org.ldrop.Ldrop1 /org/ldrop/Ldrop1 org.ldrop.Ldrop1.Debug \
    InjectIncoming ssast "Pixel 9" android 2 photo.jpg notes.pdf 4000000
```

Stop the installed service first (`systemctl --user stop ldropd`): only one daemon owns the bus
name. The design and milestones are in [SPEC.md](SPEC.md).

### Layout

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
| `crates/ldrop-gui` | `ldrop-gui`: tray icon, notifications, share sheet (Rust + cxx-qt, QML in `qml/`) |
| `integrations/` | File manager entries (Dolphin, Nautilus, Nemo, Caja, PCManFM-Qt) and the KDE Share menu plugin |
| `docs/` | Recipes for window managers ([docs/wm.md](docs/wm.md)) |
| `packaging/` | systemd user unit, D-Bus activation file, desktop entry, Plasma tray applet, API introspection XML |

### Tests

```sh
cargo test --workspace
```

The D-Bus tests in `crates/ldropd/tests/` start a private `dbus-daemon` and are skipped when it
is not installed. The TUI compares every screen against text snapshots in
`crates/ldrop-tui/snapshots/`; after an intended change to the layout, rewrite them with
`LDROP_UPDATE_SNAPSHOTS=1 cargo test -p ldrop-tui` and review the diff.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
