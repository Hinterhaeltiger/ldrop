# ldrop

Quick Share and AirDrop for Linux: send to and receive from Android phones and Apple devices,
peer to peer. The design is in [SPEC.md](SPEC.md).

**Status: milestone M1 (skeleton).** The daemon, D-Bus API, client library, CLI and `ldrop
doctor` work. No protocol backend exists yet; to try the frontends, run the daemon with
simulated peers (`--dev-sim`).

## Layout

| Path | What |
|---|---|
| `crates/ldrop-core` | Shared types, config, history, capability rules, D-Bus wire types |
| `crates/ldrop-wifi` | nl80211 Wi-Fi capability probe |
| `crates/ldrop-ble` | BlueZ capability probe |
| `crates/ldropd` | User daemon, owns `org.ldrop.Ldrop1` on the session bus |
| `crates/ldrop-client` | Client library for frontends: proxies, discovery, transfers, Agent |
| `crates/ldrop-cli` | `ldrop` command |
| `packaging/` | systemd user unit, D-Bus activation file, API introspection XML |

## Build and try

```sh
cargo build
target/debug/ldropd --dev-sim &      # simulated "Sim Pixel" and "Sim iPhone"
target/debug/ldrop doctor            # what this machine's hardware supports
target/debug/ldrop devices
target/debug/ldrop send --to "Sim Pixel" README.md
target/debug/ldrop receive           # in another terminal, then:
busctl --user call org.ldrop.Ldrop1 /org/ldrop/Ldrop1 org.ldrop.Ldrop1.Debug \
    InjectIncoming ssast "Pixel 9" android 1 photo.jpg 2000000
```

Logging: `LDROP_LOG=debug`.

## Tests

```sh
cargo test --workspace
```

The D-Bus tests in `crates/ldropd/tests/` start a private `dbus-daemon` and are skipped when it
is not installed.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
