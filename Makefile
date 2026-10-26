# SPDX-License-Identifier: GPL-3.0-or-later
#
#   make                    build everything (as your user)
#   sudo make install       install for all users, under PREFIX (default /usr/local)
#   make install-user       install for your user only, under ~/.local (no root)
#   sudo make uninstall     / make uninstall-user
#
# Installs the programs, the systemd user unit, the D-Bus service, the desktop and autostart
# entries, the Plasma applet, the GNOME Shell extension, the file manager entries and, when
# their libraries were found at build time, the native Nautilus extension and the KDE Share
# menu plugin. ldrop-gui does the per-user rest on its first start (SPEC §11.6). Pass the same
# PREFIX to make and make install. Packagers: set DESTDIR and PREFIX=/usr.

PREFIX       ?= /usr/local
BINDIR       ?= $(PREFIX)/bin
DATADIR      ?= $(PREFIX)/share
LIBDIR       ?= $(PREFIX)/lib
SYSCONFDIR   ?= $(if $(filter /usr,$(PREFIX)),/etc,$(if $(filter /usr/local,$(PREFIX)),/etc,$(PREFIX)/etc))
SYSTEMDUSERDIR ?= $(LIBDIR)/systemd/user
AUTOSTARTDIR ?= $(SYSCONFDIR)/xdg/autostart
# KDE Share menu plugin; it has to sit in Qt's own plugin directory to be found.
QT_PLUGINDIR ?= $(shell qtpaths6 --query QT_INSTALL_PLUGINS 2>/dev/null || qmake6 -query QT_INSTALL_PLUGINS 2>/dev/null || echo $(LIBDIR)/qt6/plugins)
PURPOSE      ?= yes
# Native Nautilus extension; Nautilus only loads extensions from its own directory.
NAUTILUS_EXTDIR ?= $(shell pkg-config --variable=extensiondir libnautilus-extension-4 2>/dev/null)
NAUTILUS     ?= yes

CARGO   ?= cargo
INSTALL ?= install
BINS    := ldropd ldrop ldrop-tui ldrop-gui
RELEASE := target/release
PURPOSE_BUILD := build/purpose
NAUTILUS_LIB  := build/libldrop-nautilus.so
CC     ?= cc
CFLAGS ?= -O2 -g

# The packaged files name /usr/bin and bare program names; point them at BINDIR.
SUBST = sed -e 's|/usr/bin/ldropd|$(BINDIR)/ldropd|g' \
	-e 's|Exec=ldrop|Exec=$(BINDIR)/ldrop|g' \
	-e 's|"ldrop-gui"|"$(BINDIR)/ldrop-gui"|g'

.PHONY: all build purpose nautilus install install-user uninstall uninstall-user check

all: build purpose nautilus

build:
	$(CARGO) build --release --locked

# Optional: skipped with a note when CMake or KDE's Purpose development files are missing.
purpose:
ifeq ($(PURPOSE),yes)
	@mkdir -p build; if ! command -v cmake >/dev/null; then \
		echo "note: cmake not found, skipping the KDE Share menu plugin"; \
	elif cmake -S integrations/purpose -B $(PURPOSE_BUILD) -DCMAKE_BUILD_TYPE=Release \
			-DCMAKE_INSTALL_PREFIX=$(PREFIX) -DKDE_INSTALL_PLUGINDIR=$(QT_PLUGINDIR) \
			>$(PURPOSE_BUILD).log 2>&1; then \
		cmake --build $(PURPOSE_BUILD) >>$(PURPOSE_BUILD).log 2>&1 \
			|| { cat $(PURPOSE_BUILD).log; exit 1; }; \
	else \
		rm -rf $(PURPOSE_BUILD); \
		echo "note: KF6 Purpose not found (see $(PURPOSE_BUILD).log), skipping the KDE Share menu plugin"; \
	fi
endif

# Optional: without libnautilus-extension-4, Nautilus gets the nautilus-python extension
# instead (and ldrop-gui adds a Scripts entry when nautilus-python is missing too).
nautilus:
ifeq ($(NAUTILUS),yes)
	@mkdir -p build; if pkg-config --exists libnautilus-extension-4 2>/dev/null; then \
		echo "$(CC) ... -o $(NAUTILUS_LIB)"; \
		$(CC) $(CFLAGS) -std=gnu11 -Wall -Wextra -Wno-unused-parameter -shared -fPIC \
			-DLDROP_GUI='"$(BINDIR)/ldrop-gui"' -o $(NAUTILUS_LIB) integrations/nautilus/ldrop-nautilus.c \
			$$(pkg-config --cflags --libs libnautilus-extension-4) $(LDFLAGS); \
	else \
		rm -f $(NAUTILUS_LIB); \
		echo "note: libnautilus-extension-4 not found, Nautilus gets the nautilus-python extension instead"; \
	fi
endif

check:
	$(CARGO) test --workspace --locked

install:
	@for b in $(BINS); do test -x $(RELEASE)/$$b || { echo "$(RELEASE)/$$b is missing: run make first (not as root)"; exit 1; }; done
	$(INSTALL) -Dm755 -t $(DESTDIR)$(BINDIR) $(addprefix $(RELEASE)/,$(BINS))
	@# Daemon: systemd user unit, D-Bus activation, API description
	$(INSTALL) -d $(DESTDIR)$(SYSTEMDUSERDIR) $(DESTDIR)$(DATADIR)/dbus-1/services $(DESTDIR)$(DATADIR)/dbus-1/interfaces
	$(SUBST) packaging/systemd/user/ldropd.service > $(DESTDIR)$(SYSTEMDUSERDIR)/ldropd.service
	$(SUBST) packaging/dbus/services/org.ldrop.Ldrop1.service > $(DESTDIR)$(DATADIR)/dbus-1/services/org.ldrop.Ldrop1.service
	$(INSTALL) -m644 packaging/dbus/interfaces/org.ldrop.Ldrop1.xml $(DESTDIR)$(DATADIR)/dbus-1/interfaces/
	@# Launcher ("Open with ldrop") and tray autostart
	$(INSTALL) -d $(DESTDIR)$(DATADIR)/applications $(DESTDIR)$(AUTOSTARTDIR)
	$(SUBST) packaging/desktop/org.ldrop.Ldrop.desktop > $(DESTDIR)$(DATADIR)/applications/org.ldrop.Ldrop.desktop
	$(SUBST) packaging/autostart/org.ldrop.Ldrop.desktop > $(DESTDIR)$(AUTOSTARTDIR)/org.ldrop.Ldrop.desktop
	@# Panel widgets: Plasma applet, GNOME Shell extension
	$(INSTALL) -Dm644 -t $(DESTDIR)$(DATADIR)/plasma/plasmoids/org.ldrop.visibility packaging/plasma/org.ldrop.visibility/metadata.json
	$(INSTALL) -Dm644 -t $(DESTDIR)$(DATADIR)/plasma/plasmoids/org.ldrop.visibility/contents/ui packaging/plasma/org.ldrop.visibility/contents/ui/main.qml
	$(INSTALL) -Dm644 -t $(DESTDIR)$(DATADIR)/gnome-shell/extensions/ldrop@ldrop.org integrations/gnome-shell/ldrop@ldrop.org/metadata.json integrations/gnome-shell/ldrop@ldrop.org/extension.js
	@# "Send with ldrop" in file managers (KIO wants service menus executable)
	$(INSTALL) -d $(DESTDIR)$(DATADIR)/kio/servicemenus $(DESTDIR)$(DATADIR)/nemo/actions $(DESTDIR)$(DATADIR)/file-manager/actions
	$(SUBST) integrations/dolphin/org.ldrop.Ldrop.share.desktop > $(DESTDIR)$(DATADIR)/kio/servicemenus/org.ldrop.Ldrop.share.desktop
	chmod 755 $(DESTDIR)$(DATADIR)/kio/servicemenus/org.ldrop.Ldrop.share.desktop
	@# Nautilus: the native extension, else the nautilus-python one (never both: two entries)
	@if [ "$(NAUTILUS)" = yes ] && [ -f $(NAUTILUS_LIB) ] && [ -n "$(NAUTILUS_EXTDIR)" ]; then \
		echo "$(INSTALL) -Dm755 $(NAUTILUS_LIB) $(DESTDIR)$(NAUTILUS_EXTDIR)/libldrop-nautilus.so"; \
		$(INSTALL) -Dm755 $(NAUTILUS_LIB) $(DESTDIR)$(NAUTILUS_EXTDIR)/libldrop-nautilus.so; \
		rm -f $(DESTDIR)$(DATADIR)/nautilus-python/extensions/ldrop.py; \
	else \
		$(INSTALL) -d $(DESTDIR)$(DATADIR)/nautilus-python/extensions; \
		echo "installing the nautilus-python extension"; \
		$(SUBST) integrations/nautilus/ldrop.py > $(DESTDIR)$(DATADIR)/nautilus-python/extensions/ldrop.py; \
	fi
	$(SUBST) integrations/nemo/ldrop.nemo_action > $(DESTDIR)$(DATADIR)/nemo/actions/ldrop.nemo_action
	$(SUBST) integrations/file-manager-actions/ldrop.desktop > $(DESTDIR)$(DATADIR)/file-manager/actions/ldrop.desktop
	@# KDE Share menu
	@if [ "$(PURPOSE)" = yes ] && [ -d $(PURPOSE_BUILD) ]; then \
		echo "DESTDIR=$(DESTDIR) cmake --install $(PURPOSE_BUILD)"; \
		DESTDIR=$(DESTDIR) cmake --install $(PURPOSE_BUILD) >/dev/null; \
	fi
	@if [ -z "$(DESTDIR)" ] && command -v update-desktop-database >/dev/null; then \
		update-desktop-database -q $(DATADIR)/applications || true; \
	fi
	@echo
	@echo "Installed. Next, as your user (not root):"
	@echo "  systemctl --user daemon-reload && systemctl --user enable --now ldropd"
	@echo "Then log out and back in: the tray, the Plasma applet or GNOME extension, and the"
	@echo "file manager entries start with the session."
	@if [ -z "$(DESTDIR)" ] && ! for d in /usr/share /usr/local/share; do \
		[ -e $$d/dbus-1/system-services/io.github.t4t5.Owly.service ] && break; false; done; then \
		echo; \
		echo "Note: Owly's radio service (owlyd) is not installed, so AirDrop stays off."; \
		echo "  Arch: yay -S owly; .deb/.rpm: https://github.com/t4t5/owly/releases"; \
	fi

# Everything under ~/.local and ~/.config. The KDE Share menu plugin and the native Nautilus
# extension need system directories, so they are left out here.
XDG_DATA_HOME   ?= $(HOME)/.local/share
XDG_CONFIG_HOME ?= $(HOME)/.config
USER_VARS = PREFIX=$(HOME)/.local DATADIR=$(XDG_DATA_HOME) SYSTEMDUSERDIR=$(XDG_DATA_HOME)/systemd/user \
	AUTOSTARTDIR=$(XDG_CONFIG_HOME)/autostart PURPOSE=no NAUTILUS=no

install-user:
	@test "$$(id -u)" != 0 || { echo "install-user is for your own account; use sudo make install for all users"; exit 1; }
	$(MAKE) --no-print-directory install $(USER_VARS)
	-systemctl --user daemon-reload
	@echo "(~/.local/bin should be on your PATH for the ldrop commands)"

uninstall:
	rm -f $(addprefix $(DESTDIR)$(BINDIR)/,$(BINS))
	rm -f $(DESTDIR)$(SYSTEMDUSERDIR)/ldropd.service
	rm -f $(DESTDIR)$(DATADIR)/dbus-1/services/org.ldrop.Ldrop1.service $(DESTDIR)$(DATADIR)/dbus-1/interfaces/org.ldrop.Ldrop1.xml
	rm -f $(DESTDIR)$(DATADIR)/applications/org.ldrop.Ldrop.desktop $(DESTDIR)$(AUTOSTARTDIR)/org.ldrop.Ldrop.desktop
	rm -rf $(DESTDIR)$(DATADIR)/plasma/plasmoids/org.ldrop.visibility $(DESTDIR)$(DATADIR)/gnome-shell/extensions/ldrop@ldrop.org
	rm -f $(DESTDIR)$(DATADIR)/kio/servicemenus/org.ldrop.Ldrop.share.desktop $(DESTDIR)$(DATADIR)/nautilus-python/extensions/ldrop.py
	rm -f $(DESTDIR)$(DATADIR)/nemo/actions/ldrop.nemo_action $(DESTDIR)$(DATADIR)/file-manager/actions/ldrop.desktop
	@if [ "$(PURPOSE)" = yes ]; then rm -fv $(DESTDIR)$(QT_PLUGINDIR)/kf6/purpose/ldropplugin.so; fi
	@if [ "$(NAUTILUS)" = yes ] && [ -n "$(NAUTILUS_EXTDIR)" ]; then rm -fv $(DESTDIR)$(NAUTILUS_EXTDIR)/libldrop-nautilus.so; fi

uninstall-user:
	$(MAKE) --no-print-directory uninstall $(USER_VARS)
