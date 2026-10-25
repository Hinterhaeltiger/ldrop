// SPDX-License-Identifier: GPL-3.0-or-later
//! Domain types (SPEC §8, §9). Every enum has a stable lowercase string form, used on D-Bus,
//! in JSON output and in the history file.

use serde::{Deserialize, Serialize};

/// Error for a string that names no variant of a [`string_enum!`] type.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown {kind} `{value}`")]
pub struct ParseEnumError {
    pub kind: &'static str,
    pub value: String,
}

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident ($kind:literal) { $($variant:ident => $s:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, ::serde::Serialize, ::serde::Deserialize)]
        pub enum $name {
            $(#[serde(rename = $s)] $variant),+
        }

        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $s),+
                }
            }
        }

        impl ::std::str::FromStr for $name {
            type Err = $crate::model::ParseEnumError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($s => Ok($name::$variant),)+
                    _ => Err($crate::model::ParseEnumError { kind: $kind, value: s.to_owned() }),
                }
            }
        }

        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.pad(self.as_str())
            }
        }
    };
}
pub(crate) use string_enum;

string_enum!(
    /// Device form factor, as shown in share sheets.
    DeviceKind ("device kind") {
        Phone => "phone",
        Tablet => "tablet",
        Laptop => "laptop",
        Desktop => "desktop",
        Car => "car",
        Xr => "xr",
        Unknown => "unknown",
    }
);

string_enum!(
    Platform ("platform") {
        Android => "android",
        Apple => "apple",
        Linux => "linux",
    }
);

string_enum!(
    Protocol ("protocol") {
        QuickShare => "quickshare",
        AirDrop => "airdrop",
    }
);

impl Platform {
    /// Name shown to users.
    pub fn label(self) -> &'static str {
        match self {
            Platform::Android => "Android",
            Platform::Apple => "Apple",
            Platform::Linux => "Linux",
        }
    }
}

impl Protocol {
    /// Name shown to users.
    pub fn label(self) -> &'static str {
        match self {
            Protocol::QuickShare => "Quick Share",
            Protocol::AirDrop => "AirDrop",
        }
    }
}

string_enum!(
    /// Link a peer was seen on or a transfer runs over.
    Medium ("medium") {
        Lan => "lan",
        Bt => "bt",
        Ble => "ble",
        Wfd => "wfd",
        Hotspot => "hotspot",
        Awdl => "awdl",
        Nan => "nan",
    }
);

string_enum!(
    Direction ("direction") {
        Incoming => "incoming",
        Outgoing => "outgoing",
    }
);

string_enum!(
    /// Transfer state machine (SPEC §9).
    TransferState ("transfer state") {
        Connecting => "connecting",
        AwaitingAcceptance => "awaiting-acceptance",
        Transferring => "transferring",
        Completed => "completed",
        Rejected => "rejected",
        Cancelled => "cancelled",
        Failed => "failed",
    }
);

impl TransferState {
    pub fn is_final(self) -> bool {
        matches!(
            self,
            TransferState::Completed | TransferState::Rejected | TransferState::Cancelled | TransferState::Failed
        )
    }
}

string_enum!(
    /// Receive visibility (SPEC §1.1). `contacts` and `your-devices` are parsed by
    /// [`Visibility::parse_request`] only to reject them with `NotSupported`.
    Visibility ("visibility") {
        Hidden => "hidden",
        Everyone => "everyone",
    }
);

/// Why a visibility request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VisibilityError {
    #[error("visibility `{0}` is not supported (needs vendor account certificates)")]
    NotSupported(String),
    #[error(transparent)]
    Unknown(#[from] ParseEnumError),
}

impl Visibility {
    pub fn parse_request(s: &str) -> Result<Self, VisibilityError> {
        match s {
            "contacts" | "your-devices" => Err(VisibilityError::NotSupported(s.to_owned())),
            _ => Ok(s.parse()?),
        }
    }
}

/// A nearby device that can receive (SPEC §8).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    /// Opaque; stable only within one discovery session.
    pub id: String,
    pub name: String,
    pub kind: DeviceKind,
    pub platform: Platform,
    pub protocol: Protocol,
    pub mediums: Vec<Medium>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
}

/// The other side of a transfer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Peer {
    pub name: String,
    pub platform: Platform,
    pub kind: DeviceKind,
    pub protocol: Protocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
}

pub const MIME_TEXT: &str = "text/plain";
pub const MIME_URL: &str = "text/uri-list";
/// Received Wi-Fi credentials: `name` is the SSID, `text` the password. Never applied automatically.
pub const MIME_WIFI: &str = "application/x-wifi-credentials";

/// One file, text or URL in a transfer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    pub name: String,
    pub mime: String,
    pub size: u64,
    /// Final location of a received file, or the source of a sent one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Content of a text, URL or Wi-Fi item; `Some("")` while an incoming one has not arrived
    /// yet, which marks the item as text. Never written to the history file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl Item {
    pub fn is_url(&self) -> bool {
        self.mime == MIME_URL
    }
}

/// Human-readable byte count, e.g. `12.3 MB` (decimal units, as Android shows them).
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value < 10.0 { format!("{value:.1} {}", UNITS[unit]) } else { format!("{value:.0} {}", UNITS[unit]) }
}

/// "3 files", "1 file", "a link", "some text", "a Wi-Fi network": the noun phrase prompts use.
pub fn describe_items(items: &[Item]) -> String {
    match items {
        [] => "nothing".to_owned(),
        [one] if one.is_url() => "a link".to_owned(),
        [one] if one.mime == MIME_TEXT && one.text.is_some() => "some text".to_owned(),
        [one] if one.mime == MIME_WIFI => "a Wi-Fi network".to_owned(),
        [_] => "1 file".to_owned(),
        many => format!("{} files", many.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip_through_strings() {
        for k in DeviceKind::ALL {
            assert_eq!(k.as_str().parse::<DeviceKind>().unwrap(), *k);
        }
        for s in TransferState::ALL {
            assert_eq!(s.as_str().parse::<TransferState>().unwrap(), *s);
        }
        assert_eq!(serde_json::to_string(&TransferState::AwaitingAcceptance).unwrap(), "\"awaiting-acceptance\"");
        assert!("phablet".parse::<DeviceKind>().is_err());
    }

    #[test]
    fn unsupported_visibility_modes_are_distinguished() {
        assert_eq!(Visibility::parse_request("everyone"), Ok(Visibility::Everyone));
        assert!(matches!(Visibility::parse_request("contacts"), Err(VisibilityError::NotSupported(_))));
        assert!(matches!(Visibility::parse_request("your-devices"), Err(VisibilityError::NotSupported(_))));
        assert!(matches!(Visibility::parse_request("nobody"), Err(VisibilityError::Unknown(_))));
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(format_size(999), "999 B");
        assert_eq!(format_size(2_400_000), "2.4 MB");
        assert_eq!(format_size(12_000_000), "12 MB");
        assert_eq!(format_size(4_000_000_000), "4.0 GB");
    }

    #[test]
    fn item_descriptions() {
        let file = Item { name: "a.jpg".into(), mime: "image/jpeg".into(), size: 1, ..Default::default() };
        let url =
            Item { name: "x".into(), mime: MIME_URL.into(), text: Some("https://x".into()), ..Default::default() };
        assert_eq!(describe_items(std::slice::from_ref(&file)), "1 file");
        assert_eq!(describe_items(&[file.clone(), file]), "2 files");
        assert_eq!(describe_items(&[url]), "a link");
        let pending_text =
            Item { name: "Hi".into(), mime: MIME_TEXT.into(), text: Some(String::new()), ..Default::default() };
        assert_eq!(describe_items(&[pending_text]), "some text");
        let txt_file = Item { name: "notes.txt".into(), mime: MIME_TEXT.into(), ..Default::default() };
        assert_eq!(describe_items(&[txt_file]), "1 file");
        let wifi = Item { name: "Home".into(), mime: MIME_WIFI.into(), ..Default::default() };
        assert_eq!(describe_items(&[wifi]), "a Wi-Fi network");
    }
}
