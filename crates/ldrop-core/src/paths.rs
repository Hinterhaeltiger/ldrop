// SPDX-License-Identifier: GPL-3.0-or-later
//! File locations (SPEC §9, §10, §12). Passed around explicitly so tests can isolate them.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    /// `$XDG_CONFIG_HOME/ldrop/config.toml`
    pub config_file: PathBuf,
    /// `$XDG_STATE_HOME/ldrop`
    pub state_dir: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
        let config = dirs::config_dir().unwrap_or_else(|| home.join(".config"));
        let state = dirs::state_dir().unwrap_or_else(|| home.join(".local/state"));
        Paths::under(&config, &state)
    }

    /// Paths rooted at explicit config and state base directories.
    pub fn under(config_home: &Path, state_home: &Path) -> Self {
        Paths { config_file: config_home.join("ldrop/config.toml"), state_dir: state_home.join("ldrop") }
    }

    pub fn history_file(&self) -> PathBuf {
        self.state_dir.join("history.jsonl")
    }
}

/// Where received files go: the configured dir, else the XDG download dir, else `~/Downloads`.
pub fn resolve_download_dir(configured: &str) -> PathBuf {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    if !configured.is_empty() {
        return expand_tilde(configured, &home);
    }
    dirs::download_dir().unwrap_or_else(|| home.join("Downloads"))
}

fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Some("") => home.to_path_buf(),
        Some(rest) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tilde_expansion() {
        let home = Path::new("/home/u");
        assert_eq!(expand_tilde("~/Inbox", home), PathBuf::from("/home/u/Inbox"));
        assert_eq!(expand_tilde("~", home), PathBuf::from("/home/u"));
        assert_eq!(expand_tilde("/srv/in", home), PathBuf::from("/srv/in"));
        assert_eq!(expand_tilde("~other/x", home), PathBuf::from("~other/x"));
    }

    #[test]
    fn configured_dir_wins() {
        assert_eq!(resolve_download_dir("/srv/in"), PathBuf::from("/srv/in"));
    }
}
