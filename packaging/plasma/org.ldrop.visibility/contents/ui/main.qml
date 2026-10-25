/*
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * ldrop in Plasma's system tray: the visibility switch (SPEC §11.4, tray), as a native
 * applet. It talks to ldropd over D-Bus directly; the tray loads it while ldropd runs.
 */

pragma ComponentBehavior: Bound

import QtQuick
import QtQuick.Layouts

import org.kde.kirigami as Kirigami
import org.kde.plasma.components as PlasmaComponents3
import org.kde.plasma.core as PlasmaCore
import org.kde.plasma.extras as PlasmaExtras
import org.kde.plasma.plasmoid
import org.kde.plasma.workspace.dbus as DBus

PlasmoidItem {
    id: root

    readonly property string service: "org.ldrop.Ldrop1"
    readonly property string objectPath: "/org/ldrop/Ldrop1"
    readonly property string managerIface: "org.ldrop.Ldrop1.Manager"

    // Copies of ldropd's properties. Bindings to keys of manager.properties miss keys that only
    // appear after the first fetch, so sync() copies them on every change instead.
    property string visibility: ""
    property string deviceName: ""
    property string downloadDir: ""
    // VisibilityExpiresAt is in microseconds since the epoch, 0 when there is no deadline.
    property real expiresAtMs: 0

    readonly property bool running: watcher.registered && visibility !== ""
    readonly property bool visibleToEveryone: running && visibility === "everyone"
    property real now: Date.now()
    readonly property int secondsLeft: visibleToEveryone && expiresAtMs > 0
        ? Math.max(0, Math.ceil((expiresAtMs - now) / 1000)) : 0

    function countdown(seconds: int): string {
        const minutes = Math.floor(seconds / 60);
        const rest = seconds % 60;
        return minutes + ":" + (rest < 10 ? "0" : "") + rest;
    }

    readonly property string status: {
        if (!running) {
            return i18n("ldropd is not running");
        }
        if (!visibleToEveryone) {
            return i18n("Hidden from nearby devices");
        }
        return secondsLeft > 0
            ? i18n("Visible as %1 · %2 left", deviceName, countdown(secondsLeft))
            : i18n("Visible as %1", deviceName);
    }

    function sync() {
        const p = manager.properties;
        visibility = watcher.registered ? (p.Visibility ?? "") : "";
        deviceName = p.DeviceName ?? "";
        downloadDir = p.DownloadDir ?? "";
        expiresAtMs = (p.VisibilityExpiresAt ?? 0) / 1000;
        now = Date.now();
    }

    function setVisibility(mode: string) {
        // timeout 0: ldropd's configured default (10 minutes).
        const reply = DBus.SessionBus.asyncCall({
            service: root.service,
            path: root.objectPath,
            iface: root.managerIface,
            member: "SetVisibility",
            arguments: [new DBus.string(mode), new DBus.uint32(0)],
            signature: "(su)",
        });
        reply.finished.connect(() => {
            if (reply.isError) {
                console.warn("ldrop: cannot change visibility:", reply.error.message);
            }
        });
    }

    DBus.DBusServiceWatcher {
        id: watcher
        busType: DBus.BusType.Session
        watchedService: root.service
        onRegisteredChanged: root.sync()
    }

    DBus.Properties {
        id: manager
        busType: DBus.BusType.Session
        service: root.service
        path: root.objectPath
        iface: root.managerIface
        onRefreshed: root.sync()
        onPropertiesChanged: root.sync()
    }

    Timer {
        interval: 1000
        repeat: true
        running: root.visibleToEveryone && root.expiresAtMs > 0
        triggeredOnStart: true
        onTriggered: root.now = Date.now()
    }

    Plasmoid.icon: visibleToEveryone ? "view-visible" : "view-hidden"
    Plasmoid.title: i18n("ldrop")
    Plasmoid.status: running ? PlasmaCore.Types.ActiveStatus : PlasmaCore.Types.PassiveStatus
    toolTipMainText: i18n("ldrop")
    toolTipSubText: status

    Plasmoid.contextualActions: [
        PlasmaCore.Action {
            text: i18n("Visible to everyone")
            icon.name: "view-visible"
            checkable: true
            checked: root.visibleToEveryone
            enabled: root.running
            onTriggered: root.setVisibility(root.visibleToEveryone ? "hidden" : "everyone")
        },
        PlasmaCore.Action {
            text: i18n("Open Download Folder")
            icon.name: "folder-download"
            enabled: root.running
            onTriggered: Qt.openUrlExternally("file://" + root.downloadDir)
        }
    ]

    fullRepresentation: PlasmaExtras.Representation {
        id: full

        Layout.minimumWidth: Kirigami.Units.gridUnit * 18
        Layout.minimumHeight: content.implicitHeight + Kirigami.Units.largeSpacing * 2
        Layout.maximumHeight: Layout.minimumHeight
        collapseMarginsHint: true

        header: PlasmaExtras.PlasmoidHeading {
            RowLayout {
                anchors.fill: parent
                spacing: Kirigami.Units.smallSpacing

                Kirigami.Heading {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                    level: 1
                    text: i18n("Device visibility")
                    elide: Text.ElideRight
                }
                PlasmaComponents3.ToolButton {
                    icon.name: "folder-download"
                    enabled: root.running
                    onClicked: Qt.openUrlExternally("file://" + root.downloadDir)
                    PlasmaComponents3.ToolTip.text: i18n("Open Download Folder")
                    PlasmaComponents3.ToolTip.visible: hovered
                    Accessible.name: PlasmaComponents3.ToolTip.text
                }
            }
        }

        ColumnLayout {
            id: content
            anchors.left: parent.left
            anchors.right: parent.right
            anchors.top: parent.top
            anchors.margins: Kirigami.Units.largeSpacing
            spacing: Kirigami.Units.smallSpacing

            PlasmaExtras.PlaceholderMessage {
                Layout.fillWidth: true
                visible: !root.running
                iconName: "network-offline"
                text: i18n("ldropd is not running")
                explanation: i18n("Start it with “systemctl --user start ldropd”.")
            }

            Repeater {
                model: root.running ? [
                    {
                        mode: "hidden",
                        icon: "view-hidden",
                        title: i18n("Hidden"),
                        subtitle: i18n("No one nearby can see this computer"),
                    },
                    {
                        mode: "everyone",
                        icon: "view-visible",
                        title: i18n("Everyone for 10 minutes"),
                        subtitle: root.visibleToEveryone && root.secondsLeft > 0
                            ? i18n("Visible as %1 · %2 left", root.deviceName, root.countdown(root.secondsLeft))
                            : i18n("Nearby Android phones, iPhones and Macs can send you files"),
                    },
                ] : []

                delegate: PlasmaComponents3.ItemDelegate {
                    id: option

                    required property var modelData
                    readonly property bool selected: (modelData.mode === "everyone") === root.visibleToEveryone

                    Layout.fillWidth: true
                    onClicked: if (!selected) root.setVisibility(modelData.mode)
                    Accessible.role: Accessible.RadioButton
                    Accessible.checked: selected
                    Accessible.name: modelData.title
                    Accessible.description: modelData.subtitle

                    contentItem: RowLayout {
                        spacing: Kirigami.Units.largeSpacing

                        Kirigami.Icon {
                            source: option.modelData.icon
                            Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                            Layout.preferredHeight: Kirigami.Units.iconSizes.medium
                        }
                        ColumnLayout {
                            Layout.fillWidth: true
                            spacing: 0

                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                text: option.modelData.title
                                elide: Text.ElideRight
                            }
                            PlasmaComponents3.Label {
                                Layout.fillWidth: true
                                text: option.modelData.subtitle
                                font: Kirigami.Theme.smallFont
                                opacity: 0.7
                                wrapMode: Text.WordWrap
                            }
                        }
                        PlasmaComponents3.RadioButton {
                            checked: option.selected
                            onToggled: option.clicked()
                            Accessible.ignored: true
                        }
                    }
                }
            }
        }
    }
}
