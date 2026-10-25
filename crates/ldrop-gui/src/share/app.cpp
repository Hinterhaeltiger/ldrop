// SPDX-License-Identifier: GPL-3.0-or-later
//
// The share sheet's Qt side: a QApplication (Qt Widgets, so the desktop's QStyle and
// qqc2-desktop-style apply) and the QML engine. Everything else lives in Rust.

#include "ldrop-gui/src/share/app.h"

#include <QApplication>
#include <QIcon>
#include <QQmlApplicationEngine>
#include <QQuickStyle>
#include <QUrl>

std::int32_t run_share_app()
{
    static int argc = 1;
    static char name[] = "ldrop-gui";
    static char *argv[] = {name, nullptr};
    QApplication app(argc, argv);
    QApplication::setApplicationName(QStringLiteral("ldrop"));
    QApplication::setApplicationDisplayName(QStringLiteral("ldrop"));
    QApplication::setDesktopFileName(QStringLiteral("org.ldrop.Ldrop"));
    QApplication::setWindowIcon(QIcon::fromTheme(QStringLiteral("document-share")));

    // SPEC §11.5: never override the platform's style choice (that is what makes Union and
    // qt6ct work); off Plasma, without one, use the desktop style like KDE apps do.
    const bool plasma = qEnvironmentVariable("XDG_CURRENT_DESKTOP").split(u':').contains(QStringLiteral("KDE"));
    if (!plasma && qEnvironmentVariableIsEmpty("QT_QUICK_CONTROLS_STYLE") && qEnvironmentVariableIsEmpty("QT_QPA_PLATFORMTHEME")) {
        QQuickStyle::setStyle(QStringLiteral("org.kde.desktop"));
    }

    QQmlApplicationEngine engine;
    QObject::connect(&engine, &QQmlApplicationEngine::objectCreationFailed, &app, [] { QCoreApplication::exit(1); }, Qt::QueuedConnection);
    engine.load(QUrl(QStringLiteral("qrc:/qt/qml/org/ldrop/share/qml/ShareSheet.qml")));
    return app.exec();
}
