// SPDX-License-Identifier: GPL-3.0-or-later
//! Finished-transfer history (SPEC §10): metadata only, one JSON object per line in
//! `$XDG_STATE_HOME/ldrop/history.jsonl`, newest last, at most [`HISTORY_LEN`] kept.

use std::collections::VecDeque;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::write_atomic;
use crate::model::{Direction, Item, Peer, TransferState};

pub const HISTORY_LEN: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub direction: Direction,
    pub state: TransferState,
    pub peer: Peer,
    /// Names, sizes, MIME types and saved paths. Text content is stripped.
    pub items: Vec<Item>,
    pub bytes_total: u64,
    pub bytes_done: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub error: String,
    pub created_at: u64,
    pub finished_at: u64,
}

impl HistoryEntry {
    /// Drops everything that isn't metadata.
    pub fn sanitized(mut self) -> Self {
        for item in &mut self.items {
            item.text = None;
        }
        self
    }
}

#[derive(Debug)]
pub struct History {
    path: PathBuf,
    entries: VecDeque<HistoryEntry>,
    lines_on_disk: usize,
}

impl History {
    /// Loads the file, skipping lines that don't parse. A missing file is an empty history.
    pub fn load(path: &Path) -> Self {
        let mut entries = VecDeque::new();
        let mut lines_on_disk = 0;
        match fs::read_to_string(path) {
            Ok(text) => {
                for (n, line) in text.lines().enumerate().filter(|(_, l)| !l.trim().is_empty()) {
                    lines_on_disk += 1;
                    match serde_json::from_str::<HistoryEntry>(line) {
                        Ok(entry) => entries.push_back(entry),
                        Err(e) => tracing::warn!("{}:{}: skipping history entry: {e}", path.display(), n + 1),
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("cannot read {}: {e}", path.display()),
        }
        while entries.len() > HISTORY_LEN {
            entries.pop_front();
        }
        History { path: path.to_owned(), entries, lines_on_disk }
    }

    /// Oldest first.
    pub fn entries(&self) -> impl ExactSizeIterator<Item = &HistoryEntry> {
        self.entries.iter()
    }

    /// Appends an entry; returns the entry evicted to stay within [`HISTORY_LEN`], if any.
    pub fn push(&mut self, entry: HistoryEntry) -> io::Result<Option<HistoryEntry>> {
        let entry = entry.sanitized();
        let line = serde_json::to_string(&entry).map_err(io::Error::other)?;
        self.entries.push_back(entry);
        let evicted = (self.entries.len() > HISTORY_LEN).then(|| self.entries.pop_front()).flatten();
        if self.lines_on_disk + 1 > 2 * HISTORY_LEN {
            self.rewrite()?;
        } else {
            if let Some(dir) = self.path.parent() {
                fs::create_dir_all(dir)?;
            }
            let mut file = OpenOptions::new().create(true).append(true).open(&self.path)?;
            file.write_all(format!("{line}\n").as_bytes())?;
            self.lines_on_disk += 1;
        }
        Ok(evicted)
    }

    pub fn clear(&mut self) -> io::Result<()> {
        self.entries.clear();
        self.lines_on_disk = 0;
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }

    fn rewrite(&mut self) -> io::Result<()> {
        let mut text = String::new();
        for entry in &self.entries {
            text.push_str(&serde_json::to_string(entry).map_err(io::Error::other)?);
            text.push('\n');
        }
        write_atomic(&self.path, text.as_bytes())?;
        self.lines_on_disk = self.entries.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DeviceKind, Platform, Protocol};

    fn entry(n: u64) -> HistoryEntry {
        HistoryEntry {
            direction: Direction::Incoming,
            state: TransferState::Completed,
            peer: Peer {
                name: "Pixel".into(),
                platform: Platform::Android,
                kind: DeviceKind::Phone,
                protocol: Protocol::QuickShare,
                vendor: None,
            },
            items: vec![Item {
                name: format!("{n}.txt"),
                mime: crate::model::MIME_TEXT.into(),
                size: n,
                path: None,
                text: Some("secret".into()),
            }],
            bytes_total: n,
            bytes_done: n,
            error: String::new(),
            created_at: n,
            finished_at: n,
        }
    }

    #[test]
    fn persists_metadata_only_and_caps_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state/history.jsonl");
        let mut h = History::load(&path);
        assert_eq!(h.entries().len(), 0);

        let mut evicted = 0;
        for n in 0..(3 * HISTORY_LEN as u64) {
            evicted += h.push(entry(n)).unwrap().is_some() as usize;
        }
        assert_eq!(evicted, 2 * HISTORY_LEN);
        assert_eq!(h.entries().len(), HISTORY_LEN);

        let text = fs::read_to_string(&path).unwrap();
        assert!(!text.contains("secret"));
        assert!(text.lines().count() <= 2 * HISTORY_LEN);

        let reloaded = History::load(&path);
        assert_eq!(reloaded.entries().len(), HISTORY_LEN);
        assert_eq!(reloaded.entries().last().unwrap().bytes_total, 3 * HISTORY_LEN as u64 - 1);
    }

    #[test]
    fn corrupt_lines_are_skipped_and_clear_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.jsonl");
        let good = serde_json::to_string(&entry(1)).unwrap();
        fs::write(&path, format!("{good}\n{{broken\n\n{good}\n")).unwrap();
        let mut h = History::load(&path);
        assert_eq!(h.entries().len(), 2);
        h.clear().unwrap();
        assert!(!path.exists());
        h.clear().unwrap();
    }
}
