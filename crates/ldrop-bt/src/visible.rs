// SPDX-License-Identifier: GPL-3.0-or-later
//! The adapter renamed and discoverable while ldrop is visible, then put back.
//!
//! BlueZ stores the alias and discoverability on disk, so a crash would leave the encoded name
//! in place for good. Before changing anything, the previous values go to a marker file;
//! [`restore_after_crash`] puts them back on the next start.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zbus::OwnedObjectPath;

use crate::{Adapter, Adapter1Proxy, Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Saved {
    adapter: String,
    /// The adapter's own name; an alias equal to it means "no alias set".
    name: String,
    alias: String,
    discoverable: bool,
    discoverable_timeout: u32,
}

impl Saved {
    async fn restore(&self, adapter: &Adapter1Proxy<'_>) -> zbus::Result<()> {
        let alias = if self.alias == self.name { "" } else { self.alias.as_str() };
        // Hide first, so the encoded name is not broadcast under the old visibility.
        let hidden = adapter.set_discoverable(self.discoverable).await;
        let timeout = adapter.set_discoverable_timeout(self.discoverable_timeout).await;
        adapter.set_alias(alias).await?;
        hidden.and(timeout)
    }
}

/// The adapter under another name and discoverable. Restored by [`Visible::restore`], or in
/// the background when dropped.
pub struct Visible {
    adapter: Adapter,
    saved: Saved,
    marker: PathBuf,
    restored: bool,
}

impl Adapter {
    /// Renames the adapter to `alias` and makes it discoverable until restored. `marker` is
    /// where the previous settings are kept meanwhile.
    pub async fn make_visible(&self, alias: &str, marker: &Path) -> Result<Visible> {
        let proxy = self.proxy().await?;
        let saved = Saved {
            adapter: self.path.to_string(),
            name: proxy.name().await?,
            alias: proxy.alias().await?,
            discoverable: proxy.discoverable().await?,
            discoverable_timeout: proxy.discoverable_timeout().await?,
        };
        write_marker(marker, &saved).await?;
        let visible = Visible { adapter: self.clone(), saved, marker: marker.to_owned(), restored: false };
        let applied = async {
            proxy.set_alias(alias).await?;
            proxy.set_discoverable_timeout(0).await?;
            proxy.set_discoverable(true).await
        };
        match applied.await {
            Ok(()) => {
                tracing::debug!("Bluetooth: {} discoverable as {alias}", self.path);
                Ok(visible)
            }
            Err(e) => {
                visible.restore().await;
                Err(e.into())
            }
        }
    }
}

impl Visible {
    /// Changes the name shown while visible.
    pub async fn set_alias(&self, alias: &str) -> Result<()> {
        Ok(self.adapter.proxy().await?.set_alias(alias).await?)
    }

    pub async fn restore(mut self) {
        self.restored = true;
        restore(&self.adapter, &self.saved, &self.marker).await;
    }
}

impl Drop for Visible {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let (adapter, saved, marker) = (self.adapter.clone(), self.saved.clone(), self.marker.clone());
        rt.spawn(async move { restore(&adapter, &saved, &marker).await });
    }
}

async fn restore(adapter: &Adapter, saved: &Saved, marker: &Path) {
    let restored = async { saved.restore(&adapter.proxy().await?).await };
    match restored.await {
        Ok(()) => {
            tracing::debug!("Bluetooth: {} restored", adapter.path);
            let _ = tokio::fs::remove_file(marker).await;
        }
        // Keep the marker: the next start tries again.
        Err(e) => tracing::warn!("Bluetooth: cannot restore the adapter name and visibility: {e}"),
    }
}

/// Puts back the settings a crashed run left in `marker`, if any.
pub async fn restore_after_crash(marker: &Path) -> Result<()> {
    let text = match tokio::fs::read_to_string(marker).await {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.into()),
    };
    let Ok(saved) = serde_json::from_str::<Saved>(&text) else {
        tracing::warn!("Bluetooth: ignoring unreadable {}", marker.display());
        let _ = tokio::fs::remove_file(marker).await;
        return Ok(());
    };
    let path = OwnedObjectPath::try_from(saved.adapter.as_str())?;
    let adapter = Adapter { conn: zbus::Connection::system().await?, path };
    tracing::info!("Bluetooth: restoring the adapter name after an unclean exit");
    saved.restore(&adapter.proxy().await?).await?;
    tokio::fs::remove_file(marker).await?;
    Ok(())
}

async fn write_marker(marker: &Path, saved: &Saved) -> Result<()> {
    if let Some(dir) = marker.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let json = serde_json::to_string(saved).map_err(|e| Error::Failed(e.to_string()))?;
    let tmp = marker.with_extension("tmp");
    tokio::fs::write(&tmp, json).await?;
    tokio::fs::rename(&tmp, marker).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn markers_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("state/bt-visible.json");
        let saved = Saved {
            adapter: "/org/bluez/hci0".into(),
            name: "thinkpad".into(),
            alias: "Ole's\nlaptop".into(),
            discoverable: false,
            discoverable_timeout: 180,
        };
        write_marker(&marker, &saved).await.unwrap();
        let text = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(serde_json::from_str::<Saved>(&text).unwrap(), saved);
    }

    #[tokio::test]
    async fn no_marker_means_nothing_to_restore() {
        let dir = tempfile::tempdir().unwrap();
        restore_after_crash(&dir.path().join("missing")).await.unwrap();
        let junk = dir.path().join("junk");
        std::fs::write(&junk, "not json").unwrap();
        restore_after_crash(&junk).await.unwrap();
        assert!(!junk.exists(), "unreadable markers are dropped");
    }
}
