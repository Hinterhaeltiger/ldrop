// SPDX-License-Identifier: GPL-3.0-or-later
//! Uniform Type Identifiers, which `/Ask` gives for each file, as MIME types.

/// MIME type of a folder.
pub const MIME_DIRECTORY: &str = "inode/directory";

/// The MIME type for `uti`, or `None` when it is not a well-known one (fall back to the file
/// name then). Covers what iOS and macOS send and what `owly-airdrop` sends.
pub fn to_mime(uti: &str) -> Option<&'static str> {
    Some(match uti {
        "public.jpeg" => "image/jpeg",
        "public.png" => "image/png",
        "public.heic" => "image/heic",
        "public.heif" => "image/heif",
        "com.compuserve.gif" => "image/gif",
        "public.tiff" => "image/tiff",
        "org.webmproject.webp" => "image/webp",
        "com.apple.quicktime-movie" => "video/quicktime",
        "public.mpeg-4" => "video/mp4",
        "com.apple.m4a-audio" | "public.mpeg-4-audio" => "audio/mp4",
        "public.mp3" => "audio/mpeg",
        "com.microsoft.waveform-audio" => "audio/wav",
        "com.adobe.pdf" => "application/pdf",
        "public.plain-text" | "public.utf8-plain-text" | "public.text" => "text/plain",
        "net.daringfireball.markdown" => "text/markdown",
        "public.html" => "text/html",
        "public.rtf" => "text/rtf",
        "public.vcard" => "text/vcard",
        "com.apple.ical.ics" | "public.calendar-event" => "text/calendar",
        "public.comma-separated-values-text" => "text/csv",
        "public.json" => "application/json",
        "public.zip-archive" => "application/zip",
        "com.apple.pkpass" => "application/vnd.apple.pkpass",
        "com.microsoft.word.doc" => "application/msword",
        "org.openxmlformats.wordprocessingml.document" => {
            "application/vnd.openxmlformats-officedocument.wordprocessingml.document"
        }
        "com.microsoft.excel.xls" => "application/vnd.ms-excel",
        "org.openxmlformats.spreadsheetml.sheet" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "com.microsoft.powerpoint.ppt" => "application/vnd.ms-powerpoint",
        "org.openxmlformats.presentationml.presentation" => {
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        }
        "public.folder" | "public.directory" => MIME_DIRECTORY,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_and_unknown() {
        assert_eq!(to_mime("public.heic"), Some("image/heic"));
        assert_eq!(to_mime("com.apple.quicktime-movie"), Some("video/quicktime"));
        assert_eq!(to_mime("public.folder"), Some(MIME_DIRECTORY));
        assert_eq!(to_mime("public.data"), None);
        assert_eq!(to_mime(""), None);
    }
}
