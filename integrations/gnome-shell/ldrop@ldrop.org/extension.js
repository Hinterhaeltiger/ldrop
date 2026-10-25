// SPDX-License-Identifier: GPL-3.0-or-later
//
// ldrop in GNOME's Quick Settings: the visibility switch (SPEC §11.4, tray), the GNOME
// counterpart of the Plasma applet. GNOME has no system tray by default, so this takes the
// place of ldrop-gui's tray icon there. It talks to ldropd over D-Bus directly.

import Gio from 'gi://Gio';
import GLib from 'gi://GLib';
import GObject from 'gi://GObject';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';
import {QuickMenuToggle, SystemIndicator} from 'resource:///org/gnome/shell/ui/quickSettings.js';

const SERVICE = 'org.ldrop.Ldrop1';
const OBJECT_PATH = '/org/ldrop/Ldrop1';

const ManagerProxy = Gio.DBusProxy.makeProxyWrapper(`
<node>
  <interface name="org.ldrop.Ldrop1.Manager">
    <property name="DeviceName" type="s" access="readwrite"/>
    <property name="DownloadDir" type="s" access="readwrite"/>
    <property name="Visibility" type="s" access="read"/>
    <property name="VisibilityExpiresAt" type="t" access="read"/>
  </interface>
</node>`);

function countdown(seconds) {
    const minutes = Math.floor(seconds / 60);
    const rest = seconds % 60;
    return `${minutes}:${rest < 10 ? '0' : ''}${rest}`;
}

const VisibilityToggle = GObject.registerClass(
class VisibilityToggle extends QuickMenuToggle {
    _init() {
        super._init({
            title: 'ldrop',
            iconName: 'view-conceal-symbolic',
            toggleMode: false,
        });
        this.menu.setHeader('view-reveal-symbolic', 'Device visibility');

        this._hiddenItem = new PopupMenu.PopupMenuItem('Hidden');
        this._hiddenItem.connect('activate', () => this._setVisibility('hidden'));
        this.menu.addMenuItem(this._hiddenItem);
        this._everyoneItem = new PopupMenu.PopupMenuItem('Everyone for 10 minutes');
        this._everyoneItem.connect('activate', () => this._setVisibility('everyone'));
        this.menu.addMenuItem(this._everyoneItem);
        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        this._downloadsItem = this.menu.addAction('Open Download Folder', () => this._openDownloads());

        this.connect('clicked', () => this._setVisibility(this.checked ? 'hidden' : 'everyone'));

        this._proxy = null;
        this._cancellable = new Gio.Cancellable();
        this._tick = 0;
        // DO_NOT_AUTO_START: showing the toggle should not start ldropd; turning it on does.
        new ManagerProxy(Gio.DBus.session, SERVICE, OBJECT_PATH, (proxy, error) => {
            if (error) {
                if (!error.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                    console.warn(`ldrop: ${error.message}`);
                return;
            }
            this._proxy = proxy;
            proxy.connectObject(
                'g-properties-changed', () => this._sync(),
                'notify::g-name-owner', () => this._sync(),
                this);
            this._sync();
        }, this._cancellable, Gio.DBusProxyFlags.DO_NOT_AUTO_START);
        this._sync();
    }

    get _running() {
        return !!this._proxy?.g_name_owner && !!this._proxy.Visibility;
    }

    _sync() {
        const running = this._running;
        const everyone = running && this._proxy.Visibility === 'everyone';
        // VisibilityExpiresAt is in microseconds since the epoch, 0 without a deadline.
        const expires = running ? Number(this._proxy.VisibilityExpiresAt ?? 0) / 1000 : 0;
        const secondsLeft = everyone && expires > 0 ? Math.max(0, Math.ceil((expires - Date.now()) / 1000)) : 0;

        this.checked = everyone;
        this.iconName = everyone ? 'view-reveal-symbolic' : 'view-conceal-symbolic';
        if (!running)
            this.subtitle = 'Off';
        else if (!everyone)
            this.subtitle = 'Hidden';
        else
            this.subtitle = secondsLeft > 0 ? `Everyone · ${countdown(secondsLeft)}` : 'Everyone';

        const name = running ? this._proxy.DeviceName : '';
        this.menu.setHeader(this.iconName, 'Device visibility',
            everyone ? `Visible as ${name}` : 'Nearby phones and computers cannot see you');
        this._hiddenItem.setOrnament(running && !everyone ? PopupMenu.Ornament.CHECK : PopupMenu.Ornament.NONE);
        this._everyoneItem.setOrnament(everyone ? PopupMenu.Ornament.CHECK : PopupMenu.Ornament.NONE);
        this._everyoneItem.label.text = secondsLeft > 0
            ? `Everyone · ${countdown(secondsLeft)} left` : 'Everyone for 10 minutes';
        this._downloadsItem.sensitive = running && !!this._proxy.DownloadDir;

        const ticking = secondsLeft > 0;
        if (ticking && !this._tick) {
            this._tick = GLib.timeout_add_seconds(GLib.PRIORITY_DEFAULT, 1, () => {
                this._sync();
                return GLib.SOURCE_CONTINUE;
            });
        } else if (!ticking && this._tick) {
            GLib.source_remove(this._tick);
            this._tick = 0;
        }
    }

    _setVisibility(mode) {
        // timeout 0: ldropd's configured default (10 minutes). This call starts ldropd through
        // D-Bus activation when it is not running.
        Gio.DBus.session.call(SERVICE, OBJECT_PATH, 'org.ldrop.Ldrop1.Manager', 'SetVisibility',
            new GLib.Variant('(su)', [mode, 0]), null, Gio.DBusCallFlags.NONE, -1, this._cancellable,
            (connection, result) => {
                try {
                    connection.call_finish(result);
                } catch (e) {
                    if (!e.matches(Gio.IOErrorEnum, Gio.IOErrorEnum.CANCELLED))
                        Main.notifyError('ldrop', `Cannot change visibility: ${e.message}`);
                }
            });
    }

    _openDownloads() {
        const dir = this._proxy?.DownloadDir;
        if (!dir)
            return;
        Main.panel.closeQuickSettings();
        try {
            Gio.AppInfo.launch_default_for_uri(Gio.File.new_for_path(dir).get_uri(),
                global.create_app_launch_context(0, -1));
        } catch (e) {
            Main.notifyError('ldrop', `Cannot open ${dir}: ${e.message}`);
        }
    }

    destroy() {
        this._cancellable.cancel();
        if (this._tick)
            GLib.source_remove(this._tick);
        this._tick = 0;
        this._proxy?.disconnectObject(this);
        this._proxy = null;
        super.destroy();
    }
});

const Indicator = GObject.registerClass(
class Indicator extends SystemIndicator {
    _init() {
        super._init();
        // A top bar icon while this computer is visible, like Bluetooth's.
        this._indicator = this._addIndicator();
        this._indicator.iconName = 'view-reveal-symbolic';
        const toggle = new VisibilityToggle();
        toggle.bind_property('checked', this._indicator, 'visible', GObject.BindingFlags.SYNC_CREATE);
        this.quickSettingsItems.push(toggle);
    }

    destroy() {
        this.quickSettingsItems.forEach(item => item.destroy());
        super.destroy();
    }
});

export default class LdropExtension extends Extension {
    enable() {
        this._indicator = new Indicator();
        Main.panel.statusArea.quickSettings.addExternalIndicator(this._indicator);
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
    }
}
