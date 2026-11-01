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
    /// The device an action such as Remove… is about.
    pub device: Option<pneu_proto::Id>,
    pub blocked_pin: Option<pneu_proto::Hash>,
    pub setup_requests: Vec<super::window_setup::Decision>,
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
            device: None,
            blocked_pin: None,
            setup_requests: Vec::new(),
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
        let mut n = Self::new("Blocked an unpaired device");
        n.body = format!("{address} tried to connect");
        n.actions.push(("pair", "Add Device…"));
        n
    }
    pub fn paired(name: &str) -> Self {
        let mut n = Self::new(format!("Paired with {}", label(name)));
        n.transient = true;
        n.urgency = 0;
        n.expires = 4000;
        n
    }
    /// Once per device another of your devices added: "MacBook Air joined".
    pub fn joined(device: pneu_proto::Id, name: &str, via: &str) -> Self {
        let mut n = Self::new(format!("{} joined", label(name)));
        n.body = format!("Added via {}", escape(&label(via)));
        n.actions.push(("remove", "Remove…"));
        n.device = Some(device);
        n
    }
    pub(crate) fn readmission(record: &crate::store::members::Readmission) -> Self {
        let mut notice = Self::new(super::window_setup::readmission_text(record));
        notice.actions = vec![("deny", "Keep removed"), ("allow", "Allow")];
        notice.setup_requests = vec![super::window_setup::Decision {
            readmission: Some(record.key.clone()),
            device: record.key.device.clone(),
            setup_generation: record.key.setup_generation,
            binding_generation: 0,
        }];
        notice
    }
    pub(crate) fn link_request(
        names: &[String],
        requests: Vec<super::window_setup::Decision>,
    ) -> Self {
        let names: Vec<_> = names.iter().map(|name| label(name)).collect();
        let verb = if names.len() == 1 { "wants" } else { "want" };
        let mut notice = Self::new(format!(
            "{} {verb} to link your devices",
            super::window_setup::askers(&names)
        ));
        notice.body = "New devices then pair just once.".into();
        notice.actions = vec![("deny", "Don't Allow"), ("allow", "Allow")];
        notice.setup_requests = requests;
        notice
    }
    pub fn unpaired(device: &str) -> Self {
        let mut n = Self::new(format!(
            "{} unpaired {}",
            label(device),
            super::this_computer()
        ));
        n.actions.push(("pair", "Add Device…"));
        n
    }
    /// File arrivals always say where they came from:
    /// "From Galaxy S25+ · Copied to clipboard".
    pub fn from_device(mut self, device: &str) -> Self {
        let from = format!("From {}", escape(&label(device)));
        self.body = if self.body.is_empty() {
            from
        } else {
            format!("{from} · {}", self.body)
        };
        self
    }
    /// A clip someone sent on purpose from a device, named.
    pub fn copied_from(mut self, device: &str) -> Self {
        self.summary = format!("Copied from {}", label(device));
        self
    }
    pub fn image_too_large() -> Self {
        let mut n = Self::new("Image too large to send");
        n.body = "The limit for copied images is 20 MB.".into();
        n.urgency = 0;
        n.transient = true;
        n.expires = 4000;
        n
    }
    /// Short, transient: "report.pdf sent to 2 devices".
    pub fn sent(file: &str, devices: &[String]) -> Self {
        let mut n = Self::new(match devices {
            [one] => format!("{file} sent to {}", label(one)),
            many => format!("{file} sent to {} devices", many.len()),
        });
        n.urgency = 0;
        n.transient = true;
        n.expires = 4000;
        n
    }
    /// Stays until dismissed: "Couldn't send report.pdf to MacBook Air".
    pub fn send_failed(file: &str, device: &str, reason: &str) -> Self {
        let mut n = Self::new(format!("Couldn't send {file} to {}", label(device)));
        n.body = escape(&send_failure_reason(device, reason));
        n.actions.push(("retry", "Retry"));
        n
    }
    pub fn port(port: u16) -> Self {
        let mut n = Self::new("Pneu can't receive");
        n.body = format!("Port {port} is in use by another program");
        n
    }
}
// Device names are one-line labels in summaries and bodies.
fn label(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect()
}
pub(crate) fn send_failure_reason(device: &str, reason: &str) -> String {
    let device = label(device);
    match reason {
        "UnsupportedKind" | "unsupported_kind" => {
            format!("Update Pneu on {device} to send this kind of file")
        }
        "DiskFull" | "disk_full" => format!("{device}'s disk is full"),
        "NotWritable" | "not_writable" | "StorageFailed" | "storage_failed" => {
            format!("{device} can't save files right now")
        }
        _ => "Something went wrong. Try again.".into(),
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
            sensitive: false,
            hlc: None,
            source_device: None,
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
    fn oversized_image_notice_is_quiet_transient_and_actionless() {
        let n = Notice::image_too_large();
        assert_eq!(
            (n.summary.as_str(), n.body.as_str()),
            (
                "Image too large to send",
                "The limit for copied images is 20 MB."
            )
        );
        assert_eq!((n.urgency, n.transient, n.expires), (0, true, 4000));
        assert!(n.actions.is_empty() && n.urls.is_empty());
    }
    #[test]
    fn blocked_copy_is_device_neutral_and_decision_actions_are_negative_first() {
        let blocked = Notice::blocked("127.0.0.1");
        assert_eq!(blocked.summary, "Blocked an unpaired device");
        assert_eq!(blocked.actions, vec![("pair", "Add Device…")]);
        let request = Notice::link_request(&["Computer".into()], Vec::new());
        assert_eq!(
            request.actions,
            vec![("deny", "Don't Allow"), ("allow", "Allow")]
        );
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
    fn several_devices_name_the_sender_and_results_name_recipients() {
        let n = Notice::file(
            &metadata(FileKind::Screenshot),
            true,
            true,
            false,
            None,
            "~/Pictures/Pneu",
            super::super::ReceiveDestination::SaveFolder,
        )
        .from_device("Galaxy\nS25+");
        assert_eq!(n.body, "From Galaxy S25+ · Copied to clipboard");
        let mut file = metadata(FileKind::File);
        file.file_name = "Quarterly report final v3.pdf".into();
        let n = Notice::file(
            &file,
            true,
            false,
            false,
            None,
            "",
            super::super::ReceiveDestination::SaveFolder,
        )
        .from_device("<Mac>");
        assert_eq!(
            (n.summary.as_str(), n.body.as_str()),
            ("Quarterly report final v3.pdf", "From &lt;Mac&gt;")
        );
        assert_eq!(
            Notice::text("hi").copied_from("Galaxy S25+").summary,
            "Copied from Galaxy S25+"
        );
        let sent = Notice::sent("report.pdf", &["Galaxy S25+".into(), "MacBook Air".into()]);
        assert_eq!(sent.summary, "report.pdf sent to 2 devices");
        assert!(sent.transient);
        assert_eq!(
            Notice::sent("a.png", &["MacBook Air".into()]).summary,
            "a.png sent to MacBook Air"
        );
        let failed = Notice::send_failed("report.pdf", "MacBook Air", "DiskFull");
        assert_eq!(failed.summary, "Couldn't send report.pdf to MacBook Air");
        assert_eq!(failed.body, "MacBook Air's disk is full");
        assert_eq!(failed.actions, vec![("retry", "Retry")]);
        assert!(!failed.transient);
        let joined = Notice::joined(
            pneu_proto::Id::from_bytes([7; 16]),
            "MacBook\nAir",
            "Galaxy <S25+>",
        );
        assert_eq!(
            (joined.summary.as_str(), joined.body.as_str()),
            ("MacBook Air joined", "Added via Galaxy &lt;S25+&gt;")
        );
        assert_eq!(joined.actions, vec![("remove", "Remove…")]);
        assert_eq!(joined.device, Some(pneu_proto::Id::from_bytes([7; 16])));
        assert!(!joined.transient);
        assert_eq!(
            Notice::unpaired("Office-PC").summary,
            if cfg!(target_os = "macos") {
                "Office-PC unpaired this Mac"
            } else {
                "Office-PC unpaired this computer"
            }
        );
    }
    #[test]
    fn send_failure_codes_and_legacy_stored_errors_use_plain_words() {
        for (code, expected) in [
            (
                NackCode::UnsupportedKind,
                "Update Pneu on MacBook Air to send this kind of file",
            ),
            (NackCode::DiskFull, "MacBook Air's disk is full"),
            (
                NackCode::NotWritable,
                "MacBook Air can't save files right now",
            ),
            (
                NackCode::StorageFailed,
                "MacBook Air can't save files right now",
            ),
            (NackCode::HashMismatch, "Something went wrong. Try again."),
            (NackCode::SizeMismatch, "Something went wrong. Try again."),
            (NackCode::IdConflict, "Something went wrong. Try again."),
            (
                NackCode::ClipboardUnavailable,
                "Something went wrong. Try again.",
            ),
        ] {
            let wire: String = serde_json::from_value(serde_json::to_value(code).unwrap()).unwrap();
            for reason in [wire, format!("{code:?}")] {
                assert_eq!(send_failure_reason("MacBook Air", &reason), expected);
                assert_eq!(
                    Notice::send_failed("report.pdf", "MacBook Air", &reason).body,
                    expected
                );
            }
        }
        assert_eq!(
            send_failure_reason("MacBook Air", "UnsupportedKind"),
            "Update Pneu on MacBook Air to send this kind of file"
        );
        for reason in ["", "FutureCode", "internal file path: /private/staged"] {
            assert_eq!(
                send_failure_reason("MacBook Air", reason),
                "Something went wrong. Try again."
            );
        }
        assert_eq!(
            Notice::send_failed("report.pdf", "Office <PC>&\nDesk", "NotWritable").body,
            "Office &lt;PC&gt;&amp; Desk can't save files right now"
        );
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
