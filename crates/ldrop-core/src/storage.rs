// SPDX-License-Identifier: GPL-3.0-or-later
//! Where received files land (SPEC §9, §14): sanitized flat names, a staging directory on the
//! destination filesystem, a free-space check, and moves that never overwrite.

use std::io;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

/// Kept free beyond the announced size before accepting (SPEC §9).
pub const SPACE_MARGIN: u64 = 64 * 1024 * 1024;
/// Longest name we create, in bytes: `NAME_MAX` minus room for a ` (9999)` suffix.
const MAX_NAME_LEN: usize = 255 - 7;
const MAX_EXTENSION_LEN: usize = 16;
const MAX_DUPLICATES: u32 = 9999;
const FALLBACK_NAME: &str = "file";

/// A safe, flat file name for whatever a peer sent: the last path component, without
/// control characters, never `.`/`..`/empty, at most [`MAX_NAME_LEN`] bytes with the
/// extension kept.
pub fn sanitize_name(raw: &str) -> String {
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    if clean.is_empty() || clean == "." || clean == ".." {
        return FALLBACK_NAME.to_owned();
    }
    let (stem, ext) = split_extension(clean);
    fit(stem, ext, MAX_NAME_LEN)
}

/// `("photo", ".jpg")`; dotfiles and long or odd suffixes have no extension.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 && name.len() - dot <= MAX_EXTENSION_LEN + 1 && !name[dot..].contains(' ') => {
            name.split_at(dot)
        }
        _ => (name, ""),
    }
}

fn fit(stem: &str, ext: &str, max: usize) -> String {
    if stem.len() + ext.len() <= max {
        return format!("{stem}{ext}");
    }
    let mut cut = max.saturating_sub(ext.len());
    while !stem.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{ext}", &stem[..cut])
}

/// `name`, then `name (1).ext`, `name (2).ext`, …
fn candidates(name: &str) -> impl Iterator<Item = String> + '_ {
    let (stem, ext) = split_extension(name);
    std::iter::once(name.to_owned()).chain((1..=MAX_DUPLICATES).map(move |n| format!("{stem} ({n}){ext}")))
}

/// A per-transfer directory inside the download directory, removed with everything left in it
/// when dropped. Created `0700`, so nothing in it is visible to other users before commit.
pub struct Staging {
    dir: TempDir,
}

impl Staging {
    /// Creates the download directory if needed, then `.ldrop-XXXXXX` inside it.
    pub fn create(download_dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(download_dir)?;
        let dir = tempfile::Builder::new().prefix(".ldrop-").tempdir_in(download_dir)?;
        Ok(Staging { dir })
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

/// Moves `src` into `dir` as `name` (or `name (n).ext`) without ever replacing a file.
pub fn commit(src: &Path, dir: &Path, name: &str) -> io::Result<PathBuf> {
    for candidate in candidates(name) {
        let dst = dir.join(&candidate);
        match rename_noreplace(src, &dst) {
            Ok(()) => return Ok(dst),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, format!("too many files named {name}")))
}

fn rename_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    match renameat2_noreplace(src, dst) {
        // EINVAL: the filesystem lacks the flag; ENOSYS (Unsupported): the kernel does.
        Err(e)
            if e.kind() == io::ErrorKind::Unsupported || e.raw_os_error() == Some(nix::errno::Errno::EINVAL as i32) =>
        {
            // Filesystems without RENAME_NOREPLACE: a hard link fails if the target exists.
            match std::fs::hard_link(src, dst) {
                Ok(()) => std::fs::remove_file(src),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(e),
                // No hard links either (vfat, some FUSE): check, then rename. Racy, but only
                // against the user's own processes.
                Err(_) if dst.symlink_metadata().is_ok() => Err(io::ErrorKind::AlreadyExists.into()),
                Err(_) => std::fs::rename(src, dst),
            }
        }
        other => other,
    }
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn renameat2_noreplace(src: &Path, dst: &Path) -> io::Result<()> {
    use nix::fcntl::{AT_FDCWD, RenameFlags, renameat2};
    renameat2(AT_FDCWD, src, AT_FDCWD, dst, RenameFlags::RENAME_NOREPLACE).map_err(io::Error::from)
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn renameat2_noreplace(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::ErrorKind::Unsupported.into())
}

/// Bytes available to an unprivileged user on the filesystem holding `dir`.
pub fn free_space(dir: &Path) -> io::Result<u64> {
    let st = nix::sys::statvfs::statvfs(dir).map_err(io::Error::from)?;
    Ok((st.blocks_available() as u64).saturating_mul(st.fragment_size() as u64))
}

/// Whether `bytes` fit into `dir` with [`SPACE_MARGIN`] to spare.
pub fn has_room(dir: &Path, bytes: u64) -> io::Result<bool> {
    Ok(free_space(dir)? >= bytes.saturating_add(SPACE_MARGIN))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_flat_and_safe() {
        assert_eq!(sanitize_name("photo.jpg"), "photo.jpg");
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name(r"C:\Users\x\evil.exe"), "evil.exe");
        assert_eq!(sanitize_name("a\u{0}b\nc\u{7f}.txt"), "abc.txt");
        assert_eq!(sanitize_name(".."), "file");
        assert_eq!(sanitize_name("dir/"), "file");
        assert_eq!(sanitize_name("  "), "file");
        assert_eq!(sanitize_name(".bashrc"), ".bashrc");
        assert_eq!(sanitize_name("Grüße.pdf"), "Grüße.pdf");
    }

    #[test]
    fn long_names_keep_their_extension() {
        let name = sanitize_name(&format!("{}.jpeg", "ä".repeat(200)));
        assert!(name.len() <= MAX_NAME_LEN);
        assert!(name.ends_with("ä.jpeg"));
        let odd = sanitize_name(&"x".repeat(300));
        assert_eq!(odd.len(), MAX_NAME_LEN);
    }

    #[test]
    fn duplicate_names() {
        let mut c = candidates("a.tar.gz");
        assert_eq!(c.next().unwrap(), "a.tar.gz");
        assert_eq!(c.next().unwrap(), "a.tar (1).gz");
        let mut c = candidates("README");
        c.next();
        assert_eq!(c.next().unwrap(), "README (1)");
        assert_eq!(candidates(".bashrc").nth(1).unwrap(), ".bashrc (1)");
    }

    #[test]
    fn staging_commit_never_overwrites() {
        let downloads = tempfile::tempdir().unwrap();
        let dest = downloads.path().join("Downloads");
        let staging = Staging::create(&dest).unwrap();
        assert!(staging.path().starts_with(&dest));
        std::fs::write(dest.join("a.txt"), "old").unwrap();

        let staged = staging.path().join("0");
        std::fs::write(&staged, "new").unwrap();
        let path = commit(&staged, &dest, "a.txt").unwrap();
        assert_eq!(path, dest.join("a (1).txt"));
        assert_eq!(std::fs::read_to_string(dest.join("a.txt")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(!staged.exists());

        let leftover = staging.path().join("1");
        std::fs::write(&leftover, "partial").unwrap();
        let staging_dir = staging.path().to_owned();
        drop(staging);
        assert!(!staging_dir.exists(), "staging is removed with leftovers");
    }

    #[test]
    fn free_space_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_space(dir.path()).unwrap() > 0);
        assert!(!has_room(dir.path(), u64::MAX).unwrap());
    }
}
