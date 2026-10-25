# SPDX-License-Identifier: GPL-3.0-or-later
# "Send with ldrop" in Nautilus (needs nautilus-python). Installed to
# share/nautilus-python/extensions/; Nautilus loads it on its next start (nautilus -q).

import subprocess

from gi.repository import GObject, Nautilus


class LdropMenuProvider(GObject.GObject, Nautilus.MenuProvider):
    def get_file_items(self, *args):
        # Nautilus 43+ passes (files); older versions (window, files).
        files = args[-1]
        if not files or any(f.is_directory() or f.get_uri_scheme() != "file" for f in files):
            return []
        paths = [f.get_location().get_path() for f in files]
        item = Nautilus.MenuItem(
            name="LdropMenuProvider::Send",
            label="Send with ldrop",
            tip="Send to a nearby phone or computer",
            icon="document-share",
        )
        item.connect("activate", lambda *_: subprocess.Popen(["ldrop-gui", "--share", *paths]))
        return [item]
