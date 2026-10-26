# ldrop

Quick Share and AirDrop for Linux: send to and receive from Android phones and Apple devices,
peer to peer. The design is in [SPEC.md](SPEC.md).

## What works

- **Quick Share** (Android) in both directions: files, text and links, with PIN, decline and
  cancel from either side.
  - Over the local network (mDNS discovery), or over Bluetooth when there is no shared network.
    Phones are found from their BLE advertisements, so a computer without Wi-Fi finds them too.
  - Bluetooth transfers move to Wi-Fi once they start: the same network, or a hotspot when allowed.
- **AirDrop** (iPhone, iPad, Mac), *experimental*, on the `airdrop-experimental` branch: both
  directions, files and links (and folders, when receiving), with decline and cancel. Needs a
  Wi-Fi card that [Owly](https://github.com/t4t5/omdrop-owl) supports, and Owly's radio service,
  `owlyd`. Tested end to end against Owly's AirDrop implementation on a virtual link; not yet
  against real Apple devices, so expect rough edges.
- **Frontends:**
  - `ldrop` command;
  - `ldrop-tui` terminal interface;
  - `ldrop-gui` for the tray and desktop notifications (Accept/Decline, progress);
  - a share sheet window, opened from file managers ("Send with ldrop" right in the context
    menu, or "Open with ldrop" in any of them) and the KDE Share menu;
  - a Plasma system-tray applet and a GNOME Quick Settings toggle.
- **Not yet:** Wi-Fi Direct hosting, AirDrop's Contacts Only mode, and the main GUI window.

## Requirements

- **To build:** Rust 1.89+, `protoc`, a C/C++ toolchain, and Qt 6 (for `ldrop-gui`).
  - Arch: `rust protobuf base-devel qt6-declarative`.
  - Debian/Ubuntu: `cargo protobuf-compiler build-essential qt6-declarative-dev
    qt6-base-dev`.
  - The KDE Share menu plugin also needs CMake and KDE Frameworks' Purpose: Arch `cmake
    purpose`; Debian/Ubuntu `cmake libkf6purpose-dev`.
- **To run:**
  - a D-Bus session;
  - BlueZ with a powered adapter, for Bluetooth; sending to phones found over BLE also needs
    `Experimental = true` in `/etc/bluetooth/main.conf` (see [Bluetooth](#use));
  - NetworkManager, for Wi-Fi switching (plus dnsmasq for hosting a hotspot);
  - for AirDrop, Owly's `owlyd` (Arch: `yay -S owly`; `.deb` and `.rpm` on
    [its releases page](https://github.com/t4t5/owly/releases)) and a supported Wi-Fi card;
    `ldrop doctor` says whether yours is;
  - a notification server, for the tray prompts;
  - Kirigami, for the share sheet (Arch `kirigami`, Debian/Ubuntu `qml6-module-org-kde-kirigami`).
  - Optional: Plasma 6 for the tray applet, GNOME 45 or later for the Quick Settings toggle,
    the Nautilus extension headers (Arch: part of `nautilus`; Debian/Ubuntu
    `libnautilus-extension-dev`) for *Send with ldrop* directly in Nautilus' menu. Without
    them, nautilus-python does the same, and without that ldrop adds it under *Scripts*.

## Build

```sh
git clone https://github.com/Hinterhaeltiger/ldrop
cd ldrop
make
```

This builds `ldropd` (daemon), `ldrop` (CLI), `ldrop-tui` and `ldrop-gui` in `target/release/`.
It also builds the KDE Share menu plugin when CMake and Purpose are installed, and the native
Nautilus extension when its headers are; otherwise it says so and skips them. Build as your
user, not as root.

## Install

```sh
sudo make install       # for all users, under /usr/local (PREFIX=/usr for /usr)
make install-user       # or: just for you, under ~/.local, no root
```

That puts everything in place for every desktop: the programs, the systemd user unit and D-Bus
service, the launcher and the tray autostart entry, the Plasma applet, the GNOME Shell
extension, the "Send with ldrop" entries for Dolphin, Nautilus, Nemo, Caja and PCManFM-Qt, and
the KDE Share menu plugin. The native Nautilus extension and the Share menu plugin go into
system directories, so `install-user` leaves them out. `make uninstall` (or `uninstall-user`)
takes it all out again.

A few things only exist per user. `ldrop-gui` does them once, the first time it starts in your
session, and leaves them alone afterwards if you undo them:

- on GNOME, it turns on the ldrop extension;
- with Thunar, it adds "Send with ldrop" to `~/.config/Thunar/uca.xml` (a backup goes next to
  it); `ldrop integrate thunar --remove` takes it out;
- with Nautilus but neither ldrop's Nautilus extension nor nautilus-python, it adds
  *Scripts → Send with ldrop*; `ldrop integrate nautilus-script --remove` takes it out. Once
  the extension is installed, it removes the script again.

**Upgrading from a manual install** (copied files from an older README): remove
`~/.config/systemd/user/ldropd.service` and old copies in `~/.local/bin`,
`~/.local/share/dbus-1/services` and `~/.local/share/applications`. Files there take precedence
over the ones `make install` puts in `/usr/local`.

## Set up

1. **Start the daemon** now and with every login:

   ```sh
   systemctl --user daemon-reload
   systemctl --user enable --now ldropd
   ```

   ldrop-gui and the panel widgets also start it through D-Bus when needed.
2. **Log out and back in.** The tray (or the Plasma applet, or GNOME's Quick Settings toggle)
   and the file manager entries are there in the new session. GNOME only picks up new
   extensions at login.
3. **Open the firewall** if you run one (ufw, firewalld):
   - Phones connect to this computer to send, so receiving needs inbound TCP on the Quick
     Share port, plus mDNS (5353/udp).
   - Transfers that start over Bluetooth move to the LAN on the next port up.
   - Pin the port and open both:

   ```sh
   ldrop config set quickshare.tcp_port 47000                  # then: systemctl --user restart ldropd
   sudo ufw allow 47000:47001/tcp && sudo ufw allow 5353/udp    # or: firewall-cmd --add-port=…
   ```

   Sending only needs outbound connections.

   AirDrop receives on 8771/tcp on the Apple link, `awdl0`:

   ```sh
   sudo ufw allow in on awdl0 from fe80::/10 to any port 8771 proto tcp
   ```
4. **Check the hardware.** `ldrop doctor` shows which transports this machine supports, and why
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
- *Sending:* `ldrop devices` also lists phones that are visible over Bluetooth, found from
  their BLE advertisements, so a computer without Wi-Fi finds them too. ldrop uses the LAN
  when the device is there too, and Bluetooth otherwise.
  - On the phone, set Quick Share to *Everyone*. While searching, ldrop sends a BLE wake-up
    advertisement, so the phone usually shows up within a few seconds, even with its screen
    off. Phones that are visible to contacts only don't say where to connect, so ldrop can't
    list them.
  - A phone found only over BLE is reached at the Bluetooth address it advertises, which
    needs BlueZ's experimental `ConnectDevice`: set `Experimental = true` under `[General]`
    in `/etc/bluetooth/main.conf` and restart `bluetooth.service`.
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

**AirDrop (experimental).** This part has not been tried with a real iPhone, iPad or Mac yet;
reports of what works and what doesn't are welcome. Apple devices use their own Wi-Fi link, AWDL. Owly's radio service, `owlyd`,
brings it up on the computer's Wi-Fi card (as `awdl0`) and puts Wi-Fi back afterwards, even if
ldropd crashes. ldrop asks for it only while it is needed, so Wi-Fi is left alone the rest of
the time.

- *Receiving:* make the computer visible as usual. On the iPhone, set AirDrop to *Everyone for
  10 Minutes* (Settings → General → AirDrop), then share. ldrop notices the open share sheet
  over Bluetooth, brings the Apple link up, and shows up there within a few seconds. Prompts,
  progress and downloads work as for Quick Share; links arrive as text and are never opened on
  their own. `ldrop config set airdrop.keep_link_up true` keeps the link up for the whole
  visibility window instead.
- *Sending:* `ldrop devices` (or the share sheet) lists Apple devices whose AirDrop is set to
  *Everyone*; ldrop wakes them over Bluetooth while it looks. Text that is not a link arrives as
  `Text.txt`.
- *Wi-Fi:* most cards run AWDL next to the current network, which then has to be on channel 6,
  44 or 149. Others (many Intel cards) leave the network while the link is up; ldrop allows
  that only after `ldrop config set airdrop.allow_exclusive_radio true`, and the tray then says
  "Wi-Fi paused for AirDrop".
- *Over ssh:* owlyd only lets programs in the active local session use the radio. To test from
  ssh, allow `io.github.t4t5.Owly.acquire` for your user with a polkit rule.
- Turn AirDrop off with `ldrop config set airdrop.enabled false`.

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

**Plasma.** The tray applet takes the place of the tray icon. Click it for a popup with
*Hidden* / *Everyone for 10 minutes*, the time left, and the download folder; right-click has
the same. It shows up while ldropd runs.

**GNOME.** GNOME has no tray without an extension, so ldrop brings its own: an *ldrop* toggle in
Quick Settings. Click it to switch between hidden and visible to everyone (it starts ldropd if
needed); the arrow opens *Hidden* / *Everyone for 10 minutes* with the time left, and *Open
Download Folder*. An eye icon sits in the top bar while this computer is visible. Turn it off in
the Extensions app like any other.

With the applet or the extension active, `ldrop-gui` shows no tray icon of its own and only
handles notifications; `ldrop-gui --tray-icon` brings the icon back. Other desktops and window
managers get the tray icon. The launcher entry also has *Visible to Everyone for 10 Minutes* and
*Hidden* in its right-click menu, in docks and app menus that show actions.

Only one `ldrop-gui` runs per session. The prompts need a notification server with actions:
all desktops have one, and on window managers mako, dunst or swaync do. Without one,
`ldrop-gui` leaves prompts to another frontend.

## Share sheet

`ldrop-gui --share FILE…` (or `ldrop-gui FILE…`) opens a window for sending. You get there from:

- *Send with ldrop* in the right-click menu of Dolphin, Nautilus, Nemo, Caja, Thunar and
  PCManFM-Qt;
- *Share → Send with ldrop* in KDE apps (Dolphin, Gwenview, Spectacle, Okular, …);
- *Open With → ldrop* in any file manager, for the ones above and all others.

In the window:

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
| `crates/ldrop-wifi` | nl80211 Wi-Fi capability probe, NetworkManager hotspot sessions, owlyd client |
| `crates/ldrop-ble` | BlueZ capability probe, BLE advertisements and advertisement monitors |
| `crates/ldrop-bt` | Bluetooth Classic through BlueZ: RFCOMM service, discovery, adapter visibility |
| `crates/ldrop-quickshare` | Quick Share protocol: UKEY2, SecureMessage, frames, send and receive sessions |
| `crates/ldrop-mdns` | mDNS publish and browse |
| `crates/ldrop-airdrop` | AirDrop helpers: UTI ↔ MIME, Apple model names, AWDL addresses, the BLE advert |
| `third_party/owly-airdrop` | Owly's AirDrop protocol crate, with ldrop's patches ([third_party/README.md](third_party/README.md)) |
| `crates/ldropd` | User daemon, owns `org.ldrop.Ldrop1` on the session bus |
| `crates/ldrop-client` | Client library for frontends: proxies, discovery, transfers, Agent |
| `crates/ldrop-cli` | `ldrop` command |
| `crates/ldrop-tui` | `ldrop-tui` terminal interface |
| `crates/ldrop-gui` | `ldrop-gui`: tray icon, notifications, share sheet (Rust + cxx-qt, QML in `qml/`) |
| `integrations/` | File manager entries (Dolphin, Nemo, Caja, PCManFM-Qt), the Nautilus extension (native C, and a nautilus-python fallback), the KDE Share menu plugin, the GNOME Shell extension |
| `docs/` | Recipes for window managers ([docs/wm.md](docs/wm.md)), protocol research ([docs/research/](docs/research/)) |
| `packaging/` | systemd user unit, D-Bus activation file, desktop and autostart entries, Plasma tray applet, API introspection XML |
| `Makefile` | Build and install everything (`make`, `make install`, `make install-user`) |

### Tests

```sh
cargo test --workspace
```

The D-Bus tests in `crates/ldropd/tests/` start a private `dbus-daemon` and are skipped when it
is not installed. The AirDrop tests (`tests/airdrop.rs`) also run each case in a user and
network namespace of its own, on a veth pair, against Owly's sender and receiver; they need
`unshare`, `ip` and, for some, `tc`, and no root.

To try AirDrop by hand without a supported card, run the daemon on any interface with an IPv6
link-local address: `ldropd --airdrop-interface <if>` skips owlyd and the Bluetooth wake, and
`--airdrop-peer <mac>` (repeatable) knocks at that peer as if owlyd had heard it.

The TUI compares every screen against text snapshots in
`crates/ldrop-tui/snapshots/`; after an intended change to the layout, rewrite them with
`LDROP_UPDATE_SNAPSHOTS=1 cargo test -p ldrop-tui` and review the diff.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
