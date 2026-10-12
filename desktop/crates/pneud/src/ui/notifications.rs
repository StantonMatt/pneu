//! Final notification copy, separate from D-Bus delivery and action dispatch.
use pneu_proto::{FileBegin, FileKind, NackCode};
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub summary: String,
    pub body: String,
    pub actions: Vec<(&'static str, &'static str)>,
    pub urgency: u8,
    pub transient: bool,
    pub expires: i32,
    pub urls: Vec<String>,
}
impl Notice {
    pub fn new(summary: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            body: String::new(),
            actions: Vec::new(),
            urgency: 1,
            transient: false,
            expires: -1,
            urls: Vec::new(),
        }
    }
    pub fn file(
        metadata: &FileBegin,
        saved: bool,
        copied: bool,
        failed_copy: bool,
        save_error: Option<NackCode>,
        folder: &str,
        destination: super::ReceiveDestination,
    ) -> Self {
        let mut n = Self::new(match metadata.kind {
            FileKind::Screenshot => "Screenshot",
            FileKind::Photo => "Photo",
            FileKind::Recording => "Screen recording",
            FileKind::ClipboardImage => "Image copied from phone",
            FileKind::File => &metadata.file_name,
        });
        if metadata.kind == FileKind::ClipboardImage && save_error.is_some() {
            n.summary = "Image from phone".into();
        }
        if metadata.kind == FileKind::ClipboardImage && save_error.is_none() {
            n.transient = true;
            n.urgency = 0;
            n.expires = 4000;
            if failed_copy {
                n.body = "Couldn't copy to clipboard".into();
                n.transient = false;
                n.urgency = 1;
                n.expires = -1;
            }
            return n;
        }
        if metadata.kind != FileKind::ClipboardImage {
            n.actions = vec![("default", "Open"), ("open", "Open")];
        }
        match save_error {
            Some(NackCode::DiskFull) => n.body = "Not saved: the disk is full".into(),
            Some(_) => {
                n.body = format!("Not saved: can't write to {}", escape(folder));
                if destination == super::ReceiveDestination::SaveFolder {
                    n.actions.push(("folder", "Choose Folder…"));
                }
            }
            None => {
                if copied {
                    n.body = "Copied to clipboard".into();
                }
                if failed_copy {
                    n.body = "Couldn't copy to clipboard".into();
                }
                if saved {
                    n.actions.push(("show", "Show in Folder"));
                }
            }
        }
        n
    }
    pub fn burst(count: usize, kind: Option<FileKind>, copied: bool, saved: bool) -> Self {
        let mut n = Self::new(format!(
            "{count} {}",
            match kind {
                Some(FileKind::Screenshot) => "screenshots",
                Some(FileKind::Photo) => "photos",
                _ => "items",
            }
        ));
        if copied {
            n.body = "Latest copied to clipboard".into();
        }
        if saved {
            n.actions.push(("show", "Show in Folder"));
        }
        n
    }
    pub fn text(text: &str) -> Self {
        let mut n = Self::new("Copied from phone");
        n.body = escape(
            &text
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join("\n")
                .chars()
                .take(200)
                .collect::<String>(),
        );
        if single_url(text).is_some() {
            n.actions.push(("link", "Open Link"));
        }
        n.urgency = 0;
        n.transient = true;
        n.expires = 4000;
        n
    }
    pub fn blocked(address: &str) -> Self {
        let mut n = Self::new("Blocked an unpaired phone");
        n.body = format!("{address} tried to connect");
        n.actions.push(("pair", "Pair Phone…"));
        n
    }
    pub fn unpaired(phone: &str) -> Self {
        let mut n = Self::new(format!("{phone} unpaired this PC"));
        n.actions.push(("pair", "Pair Phone…"));
        n
    }
    pub fn port(port: u16) -> Self {
        let mut n = Self::new("Pneu can't receive");
        n.body = format!("Port {port} is in use by another program");
        n
    }
}
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
pub fn single_url(text: &str) -> Option<String> {
    let text = text.trim();
    if text.chars().any(char::is_whitespace) {
        return None;
    }
    let url = url::Url::parse(text).ok()?;
    if matches!(url.scheme(), "https" | "http") && url.host_str().is_some() {
        Some(url.to_string())
    } else {
        None
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn metadata(kind: FileKind) -> FileBegin {
        FileBegin {
            transfer_id: pneu_proto::Id::from_bytes([0; 16]),
            file_name: "cat.png".into(),
            mime: "image/png".into(),
            size: 0,
            sha256: pneu_proto::Hash::new("0".repeat(64)).unwrap(),
            kind,
            created_at: 0,
            source_media_id: None,
        }
    }
    #[test]
    fn file_copy_and_save_failure_copy() {
        let n = Notice::file(
            &metadata(FileKind::Screenshot),
            true,
            true,
            false,
            None,
            "~/Pictures/Pneu",
            super::super::ReceiveDestination::SaveFolder,
        );
        assert_eq!(n.summary, "Screenshot");
        assert_eq!(n.body, "Copied to clipboard");
        assert!(n.actions.contains(&("show", "Show in Folder")));
        let n = Notice::file(
            &metadata(FileKind::Photo),
            false,
            true,
            false,
            Some(NackCode::NotWritable),
            "~/Pictures/Pneu",
            super::super::ReceiveDestination::SaveFolder,
        );
        assert_eq!(n.body, "Not saved: can't write to ~/Pictures/Pneu");
        assert!(n.actions.contains(&("folder", "Choose Folder…")));
        assert!(!n.transient);
        assert_eq!(n.urgency, 1);
        assert_eq!(
            Notice::file(
                &metadata(FileKind::Recording),
                true,
                false,
                true,
                None,
                "",
                super::super::ReceiveDestination::SaveFolder
            )
            .body,
            "Couldn't copy to clipboard"
        );
        assert_eq!(
            Notice::burst(3, Some(FileKind::Screenshot), true, true).summary,
            "3 screenshots"
        );
        assert_eq!(Notice::burst(2, None, false, false).summary, "2 items");
    }
    #[test]
    fn storage_failure_precedes_clipboard_success_and_actions_match_destination() {
        use super::super::ReceiveDestination as D;
        for kind in [
            FileKind::ClipboardImage,
            FileKind::Screenshot,
            FileKind::File,
        ] {
            for destination in [D::Cache, D::SaveFolder] {
                for code in [NackCode::NotWritable, NackCode::DiskFull] {
                    let n = Notice::file(
                        &metadata(kind),
                        false,
                        false,
                        false,
                        Some(code),
                        "/actual/destination",
                        destination,
                    );
                    assert!(!n.summary.contains("copied"));
                    assert!(n.body.starts_with("Not saved:"));
                    assert!(!n.transient);
                    assert_eq!((n.urgency, n.expires), (1, -1));
                    assert_eq!(
                        n.actions.contains(&("folder", "Choose Folder…")),
                        destination == D::SaveFolder && code != NackCode::DiskFull
                    );
                    if code == NackCode::NotWritable {
                        assert!(n.body.contains("/actual/destination"));
                    }
                }
            }
        }
    }
    #[test]
    fn text_is_bounded_escaped_and_links_are_safe() {
        let n = Notice::text("https://example.com/a?q=1&x=2");
        assert_eq!(n.actions, vec![("link", "Open Link")]);
        assert_eq!((n.urgency, n.transient, n.expires), (0, true, 4000));
        assert!(n.body.contains("&amp;"));
        assert!(Notice::text("file:///etc/passwd").actions.is_empty());
        assert!(Notice::text("https://example.com more").actions.is_empty());
        assert_eq!(Notice::text("<a>\n2\n3\n4").body, "&lt;a&gt;\n2\n3");
        assert_eq!(Notice::text(&"é".repeat(220)).body.chars().count(), 200);
    }
}
