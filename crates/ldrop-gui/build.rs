// SPDX-License-Identifier: GPL-3.0-or-later
use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    println!("cargo::rerun-if-changed=qml");
    println!("cargo::rerun-if-changed=src/share/app.cpp");
    println!("cargo::rerun-if-changed=src/share/app.h");
    CxxQtBuilder::new_qml_module(QmlModule::new("org.ldrop.share").qml_files(["qml/ShareSheet.qml"]))
        .qt_module("Gui")
        .qt_module("Widgets")
        .qt_module("Quick")
        .qt_module("QuickControls2")
        .files(["src/share/bridge.rs"])
        .cpp_file("src/share/app.cpp")
        .build();
}
