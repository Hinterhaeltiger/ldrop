// SPDX-License-Identifier: GPL-3.0-or-later
//! Generates the Quick Share protobuf types from `proto/quickshare` (needs `protoc`).

fn main() -> std::io::Result<()> {
    let dir = "../../proto/quickshare";
    let files = [
        "offline_wire_formats.proto",
        "securegcm.proto",
        "securemessage.proto",
        "device_to_device_messages.proto",
        "ukey.proto",
        "wire_format.proto",
    ]
    .map(|f| format!("{dir}/{f}"));
    println!("cargo::rerun-if-changed={dir}");
    prost_build::Config::new().include_file("protos.rs").compile_protos(&files, &[dir])
}
