// SPDX-License-Identifier: GPL-3.0-or-later
//! `config.toml` (SPEC §12): defaults, validation, unknown-key warnings, and comment-preserving
//! edits for `ldrop config set` and the D-Bus property setters.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::model::{Medium, string_enum};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read { path: String, source: io::Error },
    #[error("cannot write {path}: {source}")]
    Write { path: String, source: io::Error },
    #[error("{path}: {message}")]
    Parse { path: String, message: String },
    #[error("invalid value for `{key}`: {message}")]
    Invalid { key: String, message: String },
    #[error("unknown config key `{0}`")]
    UnknownKey(String),
}

string_enum!(
    AirDropLink ("airdrop link") {
        Auto => "auto",
        Awdl => "awdl",
        Nan => "nan",
    }
);

string_enum!(
    TuiColors ("tui colors") {
        Terminal => "terminal",
        Custom => "custom",
    }
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Empty = pretty hostname.
    pub device_name: String,
    /// Empty = XDG download dir.
    pub download_dir: String,
    pub visibility_timeout_secs: u32,
    pub quickshare: QuickShareConfig,
    pub airdrop: AirDropConfig,
    pub wifi: WifiConfig,
    pub gui: GuiConfig,
    pub tui: TuiConfig,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct QuickShareConfig {
    pub enabled: bool,
    /// 0 = random.
    pub tcp_port: u16,
    pub mediums: Vec<Medium>,
    pub prompt_timeout_secs: u32,
    /// Lets a transfer over Bluetooth move the Wi-Fi card to a phone's hotspot, or host one,
    /// although that drops the current Wi-Fi connection for its duration (SPEC §5.4).
    pub allow_wifi_switch: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AirDropConfig {
    pub enabled: bool,
    pub link: AirDropLink,
    /// Intel and others: Wi-Fi drops while the Apple link is up.
    pub allow_exclusive_radio: bool,
    pub max_upload_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WifiConfig {
    /// Empty = auto-detect.
    pub interface: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuiConfig {
    /// Empty = automatic style resolution (SPEC §11.5).
    pub style: String,
    pub start_in_tray: bool,
    pub autostart: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiConfig {
    pub colors: TuiColors,
    pub custom: TuiCustomColors,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TuiCustomColors {
    pub accent: String,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            device_name: String::new(),
            download_dir: String::new(),
            visibility_timeout_secs: 600,
            quickshare: QuickShareConfig::default(),
            airdrop: AirDropConfig::default(),
            wifi: WifiConfig::default(),
            gui: GuiConfig::default(),
            tui: TuiConfig::default(),
        }
    }
}

impl Default for QuickShareConfig {
    fn default() -> Self {
        QuickShareConfig {
            enabled: true,
            tcp_port: 0,
            mediums: vec![Medium::Lan, Medium::Wfd, Medium::Hotspot, Medium::Bt],
            prompt_timeout_secs: 60,
            allow_wifi_switch: false,
        }
    }
}

impl Default for AirDropConfig {
    fn default() -> Self {
        AirDropConfig {
            enabled: true,
            link: AirDropLink::Auto,
            allow_exclusive_radio: false,
            max_upload_bytes: 64 << 30,
        }
    }
}

impl Default for TuiConfig {
    fn default() -> Self {
        TuiConfig { colors: TuiColors::Terminal, custom: TuiCustomColors::default() }
    }
}

impl Default for TuiCustomColors {
    fn default() -> Self {
        TuiCustomColors { accent: "cyan".to_owned() }
    }
}

/// Mediums Quick Share may use (SPEC §5.4). The others are Apple-side links.
const QUICKSHARE_MEDIUMS: [Medium; 4] = [Medium::Lan, Medium::Wfd, Medium::Hotspot, Medium::Bt];

/// Longest Everyone window accepted; Android caps it at 10 min, ldrop allows up to a day.
pub const MAX_VISIBILITY_TIMEOUT_SECS: u32 = 86_400;

impl Config {
    pub fn validate(&self) -> Result<(), ConfigError> {
        let invalid =
            |key: &str, message: &str| ConfigError::Invalid { key: key.to_owned(), message: message.to_owned() };
        if !(1..=MAX_VISIBILITY_TIMEOUT_SECS).contains(&self.visibility_timeout_secs) {
            return Err(invalid("visibility_timeout_secs", "must be between 1 and 86400"));
        }
        if !(5..=600).contains(&self.quickshare.prompt_timeout_secs) {
            return Err(invalid("quickshare.prompt_timeout_secs", "must be between 5 and 600"));
        }
        let mediums = &self.quickshare.mediums;
        if mediums.is_empty() {
            return Err(invalid("quickshare.mediums", "must list at least one medium"));
        }
        if let Some(m) = mediums.iter().find(|m| !QUICKSHARE_MEDIUMS.contains(m)) {
            return Err(invalid(
                "quickshare.mediums",
                &format!("`{m}` is not a Quick Share medium (lan, wfd, hotspot, bt)"),
            ));
        }
        if mediums.iter().enumerate().any(|(i, m)| mediums[..i].contains(m)) {
            return Err(invalid("quickshare.mediums", "lists a medium twice"));
        }
        if self.airdrop.max_upload_bytes == 0 {
            return Err(invalid("airdrop.max_upload_bytes", "must be greater than 0"));
        }
        let dir = &self.download_dir;
        if !(dir.is_empty() || dir.starts_with('/') || dir.starts_with('~')) {
            return Err(invalid("download_dir", "must be an absolute path"));
        }
        Ok(())
    }

    /// Effective value of a dotted key (`quickshare.prompt_timeout_secs`) as TOML.
    pub fn get(&self, key: &str) -> Option<toml::Value> {
        let mut value = toml::Value::try_from(self).ok()?;
        for part in key.split('.') {
            value = value.as_table()?.get(part)?.clone();
        }
        Some(value)
    }
}

/// A successfully loaded config plus non-fatal findings (unknown keys).
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    pub warnings: Vec<String>,
}

/// Loads `path`. A missing file yields the defaults; an invalid one is an error, and the
/// caller keeps its last good config (SPEC §12: never reset).
pub fn load(path: &Path) -> Result<Loaded, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Loaded { config: Config::default(), warnings: Vec::new() });
        }
        Err(source) => return Err(ConfigError::Read { path: path.display().to_string(), source }),
    };
    parse(&text, &path.display().to_string())
}

pub fn parse(text: &str, origin: &str) -> Result<Loaded, ConfigError> {
    let mut warnings = Vec::new();
    let deserializer = toml::Deserializer::parse(text)
        .map_err(|e| ConfigError::Parse { path: origin.to_owned(), message: e.to_string() })?;
    let config: Config = serde_ignored::deserialize(deserializer, |key| {
        warnings.push(format!("{origin}: unknown key `{key}` ignored"));
    })
    .map_err(|e| ConfigError::Parse { path: origin.to_owned(), message: e.to_string() })?;
    config.validate()?;
    Ok(Loaded { config, warnings })
}

/// Sets a dotted key in the file at `path`, keeping comments and layout. The raw value is
/// interpreted by the type the key has in the schema (bool, integer, string or string list).
/// The file is only replaced when the result is a valid config.
pub fn set(path: &Path, key: &str, raw: &str) -> Result<Config, ConfigError> {
    let template =
        Config::default().get(key).filter(|v| !v.is_table()).ok_or_else(|| ConfigError::UnknownKey(key.to_owned()))?;
    let item = typed_item(key, &template, raw)?;
    edit(path, |doc| {
        let parts: Vec<&str> = key.split('.').collect();
        let (last, tables) = parts.split_last().expect("key is non-empty");
        let mut table = doc.as_table_mut();
        for part in tables {
            let entry = table.entry(part).or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
            table = entry.as_table_mut().ok_or_else(|| ConfigError::Invalid {
                key: key.to_owned(),
                message: format!("`{part}` is not a table in the config file"),
            })?;
        }
        table[*last] = item;
        Ok(())
    })
}

fn typed_item(key: &str, template: &toml::Value, raw: &str) -> Result<toml_edit::Item, ConfigError> {
    let invalid = |message: String| ConfigError::Invalid { key: key.to_owned(), message };
    let value: toml_edit::Value = match template {
        toml::Value::Boolean(_) => match raw {
            "true" | "yes" | "on" | "1" => true.into(),
            "false" | "no" | "off" | "0" => false.into(),
            _ => return Err(invalid(format!("expected true or false, got `{raw}`"))),
        },
        toml::Value::Integer(_) => {
            raw.parse::<i64>().map_err(|_| invalid(format!("expected an integer, got `{raw}`")))?.into()
        }
        toml::Value::Array(_) => {
            let mut array = toml_edit::Array::new();
            for part in raw.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                array.push(part);
            }
            array.into()
        }
        _ => raw.into(),
    };
    Ok(toml_edit::value(value))
}

/// Applies `change` to the parsed file and atomically replaces it if the result validates.
pub fn edit(
    path: &Path,
    change: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<(), ConfigError>,
) -> Result<Config, ConfigError> {
    let display = path.display().to_string();
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(source) => return Err(ConfigError::Read { path: display, source }),
    };
    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e: toml_edit::TomlError| ConfigError::Parse { path: display.clone(), message: e.to_string() })?;
    change(&mut doc)?;
    let new_text = doc.to_string();
    let loaded = parse(&new_text, &display)?;
    write_atomic(path, new_text.as_bytes()).map_err(|source| ConfigError::Write { path: display, source })?;
    Ok(loaded.config)
}

/// Write to a temporary sibling, fsync, rename over the target.
pub fn write_atomic(path: &Path, contents: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("config"),
        std::process::id()
    ));
    let result = (|| {
        let mut file = fs::File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.visibility_timeout_secs, 600);
        assert_eq!(c.quickshare.prompt_timeout_secs, 60);
        assert_eq!(c.quickshare.mediums, QUICKSHARE_MEDIUMS.to_vec());
        assert_eq!(c.airdrop.max_upload_bytes, 68_719_476_736);
        assert!(!c.airdrop.allow_exclusive_radio);
        assert_eq!(c.tui.custom.accent, "cyan");
        c.validate().unwrap();
    }

    #[test]
    fn empty_file_is_defaults() {
        assert_eq!(parse("", "t").unwrap().config, Config::default());
    }

    #[test]
    fn spec_example_parses() {
        let text = r#"
device_name = "ThinkPad"
download_dir = ""
visibility_timeout_secs = 600

[quickshare]
enabled = true
tcp_port = 0
mediums = ["lan", "wfd", "hotspot", "bt"]
prompt_timeout_secs = 60
allow_wifi_switch = false

[airdrop]
enabled = true
link = "auto"
allow_exclusive_radio = false
max_upload_bytes = 68719476736

[wifi]
interface = ""

[gui]
style = ""
start_in_tray = false
autostart = false

[tui]
colors = "terminal"
[tui.custom]
accent = "cyan"
"#;
        let loaded = parse(text, "t").unwrap();
        assert!(loaded.warnings.is_empty(), "{:?}", loaded.warnings);
        assert_eq!(loaded.config.device_name, "ThinkPad");
    }

    #[test]
    fn unknown_keys_warn() {
        let loaded = parse("colour = 1\n[airdrop]\nlinkk = \"awdl\"\n", "t").unwrap();
        assert_eq!(loaded.warnings.len(), 2, "{:?}", loaded.warnings);
        assert!(loaded.warnings.iter().any(|w| w.contains("`colour`")));
        assert!(loaded.warnings.iter().any(|w| w.contains("`airdrop.linkk`")));
    }

    #[test]
    fn invalid_values_are_errors() {
        assert!(parse("visibility_timeout_secs = 0", "t").is_err());
        assert!(parse("[airdrop]\nlink = \"bluetooth\"", "t").is_err());
        assert!(parse("[quickshare]\nmediums = [\"awdl\"]", "t").is_err());
        assert!(parse("[quickshare]\nmediums = [\"lan\", \"lan\"]", "t").is_err());
        assert!(parse("download_dir = \"relative/dir\"", "t").is_err());
        assert!(parse("this is not toml", "t").is_err());
    }

    #[test]
    fn get_dotted_keys() {
        let c = Config::default();
        assert_eq!(c.get("quickshare.prompt_timeout_secs"), Some(toml::Value::Integer(60)));
        assert_eq!(c.get("airdrop.link"), Some(toml::Value::String("auto".into())));
        assert_eq!(c.get("nope"), None);
    }

    #[test]
    fn set_preserves_comments_and_validates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ldrop/config.toml");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "# my notes\ndevice_name = \"Old\" # trailing\n").unwrap();

        let c = set(&path, "device_name", "New name").unwrap();
        assert_eq!(c.device_name, "New name");
        let c = set(&path, "airdrop.allow_exclusive_radio", "true").unwrap();
        assert!(c.airdrop.allow_exclusive_radio);
        let c = set(&path, "quickshare.mediums", "lan, bt").unwrap();
        assert_eq!(c.quickshare.mediums, vec![Medium::Lan, Medium::Bt]);
        set(&path, "tui.custom.accent", "magenta").unwrap();

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# my notes"), "{text}");
        assert!(text.contains("device_name = \"New name\""), "{text}");

        assert!(matches!(set(&path, "bogus", "1"), Err(ConfigError::UnknownKey(_))));
        assert!(matches!(set(&path, "airdrop", "1"), Err(ConfigError::UnknownKey(_))));
        assert!(set(&path, "visibility_timeout_secs", "0").is_err());
        assert!(set(&path, "airdrop.enabled", "maybe").is_err());
        // A rejected edit leaves the file untouched.
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
    }

    #[test]
    fn set_creates_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/config.toml");
        set(&path, "visibility_timeout_secs", "120").unwrap();
        assert_eq!(load(&path).unwrap().config.visibility_timeout_secs, 120);
    }
}
