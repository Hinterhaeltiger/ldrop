// SPDX-License-Identifier: GPL-3.0-or-later
//! `file://` URI ↔ path conversion (RFC 8089, local host only), as `Send` takes URIs.

use std::ffi::OsString;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};

/// `file:///home/u/a%20b.jpg` → `/home/u/a b.jpg`. `None` for other schemes or remote hosts.
pub fn file_uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let bytes = percent_decode(rest.as_bytes())?;
    if bytes.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsString::from_vec(bytes)))
}

/// Absolute path → `file://` URI, percent-encoding everything outside RFC 3986 `pchar`.
pub fn path_to_file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        if b.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn percent_decode(input: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' {
            let hex = input.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
            i += 3;
        } else {
            out.push(input[i]);
            i += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_with_spaces_and_unicode() {
        let path = Path::new("/home/u/Bilder/Urlaub 2026/straße #1 (100%).jpg");
        let uri = path_to_file_uri(path);
        assert_eq!(uri, "file:///home/u/Bilder/Urlaub%202026/stra%C3%9Fe%20%231%20(100%25).jpg");
        assert_eq!(file_uri_to_path(&uri).as_deref(), Some(path));
    }

    #[test]
    fn rejects_non_local_and_malformed() {
        assert_eq!(file_uri_to_path("file://localhost/tmp/x").as_deref(), Some(Path::new("/tmp/x")));
        assert_eq!(file_uri_to_path("file://server/share/x"), None);
        assert_eq!(file_uri_to_path("https://example.com/x"), None);
        assert_eq!(file_uri_to_path("file:///tmp/%zz"), None);
        assert_eq!(file_uri_to_path("file:///tmp/a%00b"), None);
    }
}
