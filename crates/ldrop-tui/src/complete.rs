// SPDX-License-Identifier: GPL-3.0-or-later
//! Tab completion for paths, shell style: complete a unique match, else extend to the longest
//! common prefix and list the candidates.

use std::path::{Path, PathBuf};

/// How many candidates are kept for display.
const MAX_CANDIDATES: usize = 64;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Completion {
    pub text: String,
    /// Names (directories end in `/`) when the match isn't unique.
    pub candidates: Vec<String>,
}

/// `~` and `~/…` against `home`; everything else as typed.
pub fn expand(path: &str, home: Option<&Path>) -> PathBuf {
    match (path.strip_prefix('~'), home) {
        (Some(""), Some(home)) => home.to_owned(),
        (Some(rest), Some(home)) if rest.starts_with('/') => home.join(&rest[1..]),
        _ => PathBuf::from(path),
    }
}

/// `/home/me/x` → `~/x`.
pub fn tilde(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|h| path.strip_prefix(h).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

pub fn complete(input: &str, home: Option<&Path>) -> Completion {
    if input == "~" {
        return Completion { text: "~/".into(), candidates: Vec::new() };
    }
    let (dir_part, prefix) = match input.rfind('/') {
        Some(i) => input.split_at(i + 1),
        None => ("", input),
    };
    let dir = if dir_part.is_empty() { PathBuf::from(".") } else { expand(dir_part, home) };
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Completion { text: input.to_owned(), candidates: Vec::new() };
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            if !name.starts_with(prefix) || (name.starts_with('.') && !prefix.starts_with('.')) {
                return None;
            }
            Some(if e.path().is_dir() { format!("{name}/") } else { name })
        })
        .collect();
    names.sort();

    match names.as_slice() {
        [] => Completion { text: input.to_owned(), candidates: Vec::new() },
        [one] => Completion { text: format!("{dir_part}{one}"), candidates: Vec::new() },
        many => {
            let text = format!("{dir_part}{}", common_prefix(many));
            names.truncate(MAX_CANDIDATES);
            Completion { text, candidates: names }
        }
    }
}

fn common_prefix(names: &[String]) -> &str {
    let first = &names[0];
    let mut end = first.len();
    for name in &names[1..] {
        // Without a mismatch, `name` is a prefix of `first[..end]` (or equal to it).
        end = first[..end]
            .char_indices()
            .zip(name.chars())
            .find(|((_, a), b)| a != b)
            .map_or(end.min(name.len()), |((i, _), _)| i);
    }
    &first[..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completes_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("Pictures")).unwrap();
        std::fs::write(root.join("photo-1.jpg"), "").unwrap();
        std::fs::write(root.join("photo-2.jpg"), "").unwrap();
        std::fs::write(root.join(".hidden"), "").unwrap();
        std::fs::write(root.join("Pictures/beach.jpg"), "").unwrap();
        let home = Some(root);

        assert_eq!(complete("~", home).text, "~/");
        assert_eq!(complete("~/Pi", home), Completion { text: "~/Pictures/".into(), candidates: vec![] });
        assert_eq!(complete("~/Pictures/b", home).text, "~/Pictures/beach.jpg");
        let many = complete("~/ph", home);
        assert_eq!(many.text, "~/photo-");
        assert_eq!(many.candidates, ["photo-1.jpg", "photo-2.jpg"]);
        assert!(!complete("~/", home).candidates.contains(&".hidden".to_owned()));
        assert_eq!(complete("~/.h", home).text, "~/.hidden");
        assert_eq!(complete("~/nothing", home).text, "~/nothing");
        let abs = format!("{}/Pic", root.display());
        assert_eq!(complete(&abs, None).text, format!("{}/Pictures/", root.display()));
    }

    #[test]
    fn common_prefix_respects_chars() {
        let names = ["grüße-a".to_owned(), "grüße-b".to_owned(), "grün".to_owned()];
        assert_eq!(common_prefix(&names), "grü");
    }

    #[test]
    fn tilde_and_expand() {
        let home = Path::new("/home/me");
        assert_eq!(tilde(Path::new("/home/me/Downloads"), Some(home)), "~/Downloads");
        assert_eq!(tilde(Path::new("/home/me"), Some(home)), "~");
        assert_eq!(tilde(Path::new("/srv/x"), Some(home)), "/srv/x");
        assert_eq!(expand("~/a", Some(home)), Path::new("/home/me/a"));
        assert_eq!(expand("~", Some(home)), home);
        assert_eq!(expand("~bob/a", Some(home)), Path::new("~bob/a"));
    }
}
