// SPDX-License-Identifier: GPL-3.0-or-later
//
// The share sheet (SPEC §11.4, surface 1): nearby devices; click one to send. The state comes
// from ShareBackend (src/share/bridge.rs) as JSON.

import QtQuick
import QtQuick.Controls as QQC2
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import org.ldrop.share

Kirigami.ApplicationWindow {
    id: window

    title: qsTr("Send with ldrop")
    width: Kirigami.Units.gridUnit * 24
    height: Kirigami.Units.gridUnit * 26
    minimumWidth: Kirigami.Units.gridUnit * 18
    minimumHeight: Kirigami.Units.gridUnit * 14
    pageStack.globalToolBar.style: Kirigami.ApplicationHeaderStyle.None

    ShareBackend {
        id: backend
    }

    readonly property var devices: JSON.parse(backend.devices)

    Component.onCompleted: backend.start()

    pageStack.initialPage: Kirigami.ScrollablePage {
        id: page

        title: window.title
        padding: 0

        header: QQC2.ToolBar {
            contentItem: ColumnLayout {
                spacing: 0

                Kirigami.Heading {
                    Layout.fillWidth: true
                    Layout.margins: Kirigami.Units.smallSpacing
                    level: 2
                    text: qsTr("Send to a nearby device")
                }
                QQC2.Label {
                    Layout.fillWidth: true
                    Layout.leftMargin: Kirigami.Units.smallSpacing
                    Layout.rightMargin: Kirigami.Units.smallSpacing
                    Layout.bottomMargin: Kirigami.Units.smallSpacing
                    text: backend.summary
                    elide: Text.ElideMiddle
                    opacity: 0.7
                }
            }
        }

        footer: QQC2.DialogButtonBox {
            standardButtons: QQC2.DialogButtonBox.Close
            onRejected: window.close()
        }

        ListView {
            id: list

            model: window.devices
            reuseItems: false

            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.gridUnit * 4
                visible: list.count === 0
                icon.name: "network-wireless"
                text: backend.status
                explanation: qsTr("Unlock the other device. On Android, set Quick Share to receive from everyone.")

                QQC2.BusyIndicator {
                    Layout.alignment: Qt.AlignHCenter
                    running: parent.visible
                }
            }

            delegate: QQC2.ItemDelegate {
                id: row

                required property var modelData
                readonly property bool idle: modelData.phase === "idle" || modelData.phase === "failed"

                width: ListView.view.width
                enabled: true
                onClicked: if (idle) backend.send(modelData.id)
                Accessible.name: modelData.name
                Accessible.description: modelData.detail

                contentItem: RowLayout {
                    spacing: Kirigami.Units.largeSpacing

                    Kirigami.Icon {
                        source: row.modelData.icon
                        Layout.preferredWidth: Kirigami.Units.iconSizes.medium
                        Layout.preferredHeight: Kirigami.Units.iconSizes.medium
                    }
                    ColumnLayout {
                        Layout.fillWidth: true
                        spacing: 0

                        QQC2.Label {
                            Layout.fillWidth: true
                            text: row.modelData.name
                            elide: Text.ElideRight
                        }
                        QQC2.Label {
                            Layout.fillWidth: true
                            text: row.modelData.detail
                            font: Kirigami.Theme.smallFont
                            color: row.modelData.phase === "failed" ? Kirigami.Theme.negativeTextColor
                                 : row.modelData.phase === "done" ? Kirigami.Theme.positiveTextColor
                                 : Kirigami.Theme.disabledTextColor
                            elide: Text.ElideRight
                        }
                        QQC2.ProgressBar {
                            Layout.fillWidth: true
                            visible: row.modelData.phase === "busy"
                            indeterminate: row.modelData.progress < 0
                            value: Math.max(0, row.modelData.progress)
                        }
                    }
                    QQC2.Button {
                        visible: row.modelData.phase === "busy"
                        text: qsTr("Cancel")
                        onClicked: backend.cancel(row.modelData.id)
                    }
                    Kirigami.Icon {
                        visible: row.modelData.phase === "done"
                        source: "checkmark"
                        Layout.preferredWidth: Kirigami.Units.iconSizes.smallMedium
                        Layout.preferredHeight: Kirigami.Units.iconSizes.smallMedium
                    }
                    QQC2.Button {
                        visible: row.idle
                        text: row.modelData.phase === "failed" ? qsTr("Retry") : qsTr("Send")
                        icon.name: "document-send"
                        onClicked: backend.send(row.modelData.id)
                    }
                }
            }
        }
    }
}
