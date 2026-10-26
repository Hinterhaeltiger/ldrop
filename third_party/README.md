# Vendored crates

## owly-airdrop

[owly-airdrop](https://crates.io/crates/owly-airdrop) 0.1.0 (MIT, see `owly-airdrop/LICENSE`),
the AirDrop layer of [Owly](https://github.com/t4t5/owly), from the published crate (upstream
commit `91ea0bd775d62c382ab90159b6ca849e91baa5fd`). The workspace's `[patch.crates-io]` points
here. ldrop's changes, all in `src/driver/`:

- `receiver.rs`: `ReceiverEvent::Progress { id, bytes }` every 256 KiB of file data written;
  `Receiver::canceller()`, whose `Canceller::cancel(id)` closes the connection serving a
  transfer; and a `Failed` event when the connection of an accepted transfer ends before its
  upload does (upstream sends nothing then).
- `sender.rs`: `Sender::spool_in(dir)` and `prepare_in(.., spool)`, so the archive built before
  an upload need not go to a small tmpfs `/tmp`; and `Event::Asking` once connected, before the
  `/Ask` waits on the receiver's user.

Nothing else differs from the published crate. When upstream has equivalents, drop the patch and
this directory.
