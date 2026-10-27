#!/bin/sh
# SPDX-License-Identifier: GPL-3.0-or-later
#
# Pack the release build into a tarball that `make install` works from without building:
#
#   make build                   (or cargo build --release --locked)
#   packaging/dist.sh VERSION    writes dist/ldrop-VERSION-ARCH-linux.tar.gz
#
# The tarball holds the programs in target/release/ next to the Makefile and the files it
# installs. The KDE Share menu plugin and the native Nautilus extension are left out: they
# depend on the installed KDE and Nautilus versions, so `make install` falls back to the
# nautilus-python extension and skips the Share menu plugin.
set -eu

VERSION=${1:?usage: packaging/dist.sh VERSION}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
ARCH=$(uname -m)
NAME=ldrop-$VERSION-$ARCH-linux
OUT=$ROOT/dist
STAGE=$OUT/$NAME

rm -rf "$STAGE"
mkdir -p "$STAGE/target/release"
for bin in ldropd ldrop ldrop-tui ldrop-gui; do
  test -x "$ROOT/target/release/$bin" || { echo "target/release/$bin is missing: build first" >&2; exit 1; }
  install -m755 "$ROOT/target/release/$bin" "$STAGE/target/release/"
done
cp -r "$ROOT/packaging" "$ROOT/integrations" "$STAGE/"
cp "$ROOT/Makefile" "$ROOT/README.md" "$ROOT/LICENSE" "$STAGE/"
rm -f "$STAGE/packaging/dist.sh"

tar -C "$OUT" -czf "$OUT/$NAME.tar.gz" "$NAME"
rm -rf "$STAGE"
(cd "$OUT" && sha256sum "$NAME.tar.gz" > "$NAME.tar.gz.sha256")
echo "$OUT/$NAME.tar.gz"
