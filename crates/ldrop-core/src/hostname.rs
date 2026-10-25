// SPDX-License-Identifier: GPL-3.0-or-later
//! Default device name: the pretty hostname (`/etc/machine-info`), else the kernel hostname.

use std::fs;

pub fn default_device_name() -> String {
    fs::read_to_string("/etc/machine-info")
        .ok()
        .and_then(|s| parse_pretty_hostname(&s))
        .or_else(|| nix::unistd::gethostname().ok().and_then(|h| h.into_string().ok()).filter(|h| !h.is_empty()))
        .unwrap_or_else(|| "Linux".to_owned())
}

fn parse_pretty_hostname(machine_info: &str) -> Option<String> {
    machine_info.lines().find_map(|line| {
        let value = line.trim().strip_prefix("PRETTY_HOSTNAME=")?;
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .or_else(|| value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
            .unwrap_or(value);
        let value = value.replace("\\\"", "\"").replace("\\\\", "\\");
        (!value.is_empty()).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_info_parsing() {
        assert_eq!(
            parse_pretty_hostname("CHASSIS=laptop\nPRETTY_HOSTNAME=\"Anna's ThinkPad\"\n"),
            Some("Anna's ThinkPad".to_owned())
        );
        assert_eq!(parse_pretty_hostname("PRETTY_HOSTNAME=box"), Some("box".to_owned()));
        assert_eq!(parse_pretty_hostname("PRETTY_HOSTNAME=\"\""), None);
        assert_eq!(parse_pretty_hostname("CHASSIS=desktop"), None);
    }
}
