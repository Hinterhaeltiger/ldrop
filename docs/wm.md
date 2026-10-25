# ldrop on window managers

Recipes for setups without a desktop shell: sway, Hyprland, i3, river, niri and the like.
Everything here talks to `ldropd` through the `ldrop` and `ldrop-tui` commands, so it works
the same everywhere.

## Terminal interface

`ldrop-tui` (or `ldrop tui`) does everything the other frontends do: pick a device and send,
answer incoming requests, follow transfers, change settings. Bind it to a key in a floating
terminal, for example in sway:

```
bindsym $mod+Shift+s exec foot --app-id ldrop -e ldrop-tui
for_window [app_id="ldrop"] floating enable, resize set 80 ppt 60 ppt
```

`ldrop-tui send FILE…` starts with the files picked, so only the device is left to choose.
That suits file managers with an "open in terminal" or custom-command entry:

```
foot -e ldrop-tui send %F
```

While it is open, `ldrop-tui` answers incoming requests itself and rings the terminal bell.

## Waybar

`ldrop status --json` prints one line in Waybar's format: `text`, `class` (`hidden`,
`everyone` or `transferring`), `tooltip`, plus all status fields.

```jsonc
"custom/ldrop": {
    "exec": "ldrop status --json",
    "return-type": "json",
    "interval": 2,
    "on-click": "[ \"$(ldrop status --json | jq -r .visibility)\" = hidden ] && ldrop visible || ldrop hidden",
    "on-click-right": "foot --app-id ldrop -e ldrop-tui"
}
```

```css
#custom-ldrop.everyone,
#custom-ldrop.transferring { color: @theme_selected_bg_color; }
```

## Device picker (rofi, fuzzel, wofi, dmenu)

`ldrop devices --json` searches for a while and prints the devices found. This script lets
you pick one and sends the files given as arguments:

```sh
#!/bin/sh
# ldrop-pick FILE…: choose a nearby device, then send FILE… to it.
device=$(ldrop devices --json --timeout 5s | jq -r '.[] | "\(.name)\t\(.platform)"' |
    fuzzel --dmenu --prompt 'Send to: ' | cut -f1)
[ -n "$device" ] && exec ldrop send --to "$device" "$@"
```

Swap `fuzzel --dmenu` for `rofi -dmenu`, `wofi --dmenu` or `dmenu`. Device ids only last
for one search, so the script passes the name.

## Incoming requests as notifications

When no `ldrop-tui` is open, `ldropd` shows each request as a desktop notification with
**Accept** and **Decline** actions and the PIN in its body. Notification daemons show actions
differently:

- **mako:** `makoctl menu` hands the actions to a picker, e.g. bind
  `makoctl menu fuzzel --dmenu` to a key.
- **dunst:** `dunstctl context` opens the action menu.
- **swaync:** shows the actions as buttons.

Without a notification daemon, answer in a terminal with `ldrop receive`, or later with
`ldrop transfers` and `ldrop accept|reject <id>`.

## Tray

A tray icon for any StatusNotifierItem host (such as Waybar's `tray` module) comes with the
GUI, `ldrop-gui --tray`, in milestone M4.
