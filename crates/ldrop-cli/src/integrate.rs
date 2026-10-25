// SPDX-License-Identifier: GPL-3.0-or-later
//! `ldrop integrate thunar`: Thunar reads custom actions only from the per-user
//! `~/.config/Thunar/uca.xml`, so "Send with ldrop" is added there (SPEC §11.6). The other file
//! managers take drop-in files, see `integrations/`.

use std::path::PathBuf;

use crate::{CmdResult, Exit, Fail};

/// Marks our action, so `--remove` finds it again.
const UNIQUE_ID: &str = "ldrop-send-with-ldrop";

const ACTION: &str = "<action>
\t<icon>document-share</icon>
\t<name>Send with ldrop</name>
\t<submenu></submenu>
\t<unique-id>ldrop-send-with-ldrop</unique-id>
\t<command>ldrop-gui --share %F</command>
\t<description>Send to a nearby phone or computer</description>
\t<range></range>
\t<patterns>*</patterns>
\t<audio-files/>
\t<image-files/>
\t<other-files/>
\t<text-files/>
\t<video-files/>
</action>
";

const EMPTY: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<actions>\n</actions>\n";

pub fn thunar(remove: bool) -> CmdResult {
    let path = uca_path();
    let current = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => EMPTY.to_owned(),
        Err(e) => return Err(Fail::new(Exit::Error, format!("cannot read {}: {e}", path.display()))),
    };
    let updated = if remove { remove_action(&current) } else { add_action(&current) };
    let updated = updated.map_err(|e| Fail::new(Exit::Error, format!("{}: {e}", path.display())))?;
    if updated == current {
        println!("Thunar already {} \"Send with ldrop\"", if remove { "lacks" } else { "has" });
        return Ok(Exit::Ok);
    }
    let write = || -> std::io::Result<()> {
        if path.exists() {
            std::fs::copy(&path, path.with_extension("xml.ldrop-backup"))?;
        } else if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, &updated)
    };
    write().map_err(|e| Fail::new(Exit::Error, format!("cannot write {}: {e}", path.display())))?;
    println!(
        "{} \"Send with ldrop\" {} {}; restart Thunar to see it: thunar -q",
        if remove { "Removed" } else { "Added" },
        if remove { "from" } else { "to" },
        path.display()
    );
    Ok(Exit::Ok)
}

fn uca_path() -> PathBuf {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_default();
    config.join("Thunar/uca.xml")
}

fn add_action(xml: &str) -> Result<String, &'static str> {
    if xml.contains(UNIQUE_ID) {
        return Ok(xml.to_owned());
    }
    let end = xml.rfind("</actions>").ok_or("not a Thunar actions file")?;
    Ok(format!("{}{ACTION}{}", &xml[..end], &xml[end..]))
}

fn remove_action(xml: &str) -> Result<String, &'static str> {
    let Some(id) = xml.find(UNIQUE_ID) else { return Ok(xml.to_owned()) };
    let start = xml[..id].rfind("<action>").ok_or("malformed action")?;
    let close = "</action>";
    let end = id + xml[id..].find(close).ok_or("malformed action")? + close.len();
    // Take the line break after it too.
    let end = if xml[end..].starts_with('\n') { end + 1 } else { end };
    Ok(format!("{}{}", &xml[..start], &xml[end..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const THUNAR_DEFAULT: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>
<actions>
<action>
\t<icon>utilities-terminal</icon>
\t<name>Open Terminal Here</name>
\t<unique-id>1-1</unique-id>
\t<command>exo-open --working-directory %f --launch TerminalEmulator</command>
\t<patterns>*</patterns>
\t<directories/>
</action>
</actions>
";

    #[test]
    fn adds_once_and_removes_cleanly() {
        let added = add_action(THUNAR_DEFAULT).unwrap();
        assert!(added.contains("<command>ldrop-gui --share %F</command>"));
        assert!(added.contains("Open Terminal Here"));
        assert!(added.trim_end().ends_with("</actions>"));
        assert_eq!(add_action(&added).unwrap(), added);
        assert_eq!(remove_action(&added).unwrap(), THUNAR_DEFAULT);
        assert_eq!(remove_action(THUNAR_DEFAULT).unwrap(), THUNAR_DEFAULT);
    }

    #[test]
    fn starts_a_missing_file() {
        let added = add_action(EMPTY).unwrap();
        assert!(added.starts_with("<?xml") && added.contains(UNIQUE_ID));
        assert!(add_action("<html></html>").is_err());
    }
}
