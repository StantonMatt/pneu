//! Opaque, session-scoped toast authority and ToastGeneric projection.
use super::{notice_lifecycle::Lifecycle, notifications::Notice};
use anyhow::{Context, Result};
use std::{collections::BTreeMap, sync::Arc, time::Duration};

pub(crate) fn can_burst(
    metadata: &pneu_proto::FileBegin,
    save_error: Option<pneu_proto::NackCode>,
) -> bool {
    metadata.folder.is_none() && save_error.is_none()
}

pub(crate) struct Record<T> {
    pub id: u32,
    pub session: String,
    pub revision: String,
    pub notice: Notice,
    pub target: T,
    pub life: Arc<Lifecycle>,
}
impl<T> Record<T> {
    pub fn argument(&self, action: &str) -> String {
        format!("{}-{}-{}-{action}", self.session, self.revision, self.id)
    }
    pub fn group(&self) -> &str {
        &self.session[..16]
    }
    pub fn xml(&self, attribution: Option<&str>) -> String {
        let default = if self.notice.actions.iter().any(|(a, _)| *a == "default") {
            "default"
        } else if self.notice.actions.iter().any(|(a, _)| *a == "show") {
            "show"
        } else {
            "window"
        };
        let mut xml = format!(
            r#"<toast activationType="foreground" launch="{}"><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text>"#,
            self.argument(default),
            escape(&self.notice.summary),
            escape(&plain(&self.notice.body)),
        );
        if let Some(name) = attribution {
            xml.push_str(&format!(
                r#"<text placement="attribution">{}</text>"#,
                escape(name)
            ));
        }
        xml.push_str("</binding></visual>");
        if self.notice.urgency == 0 {
            xml.push_str(r#"<audio silent="true"/>"#);
        }
        if !self.notice.actions.is_empty() {
            xml.push_str("<actions>");
            for (action, label) in &self.notice.actions {
                if *action != "default" {
                    xml.push_str(&format!(
                        r#"<action content="{}" arguments="{}" activationType="foreground"/>"#,
                        escape(label),
                        self.argument(action)
                    ));
                }
            }
            xml.push_str("</actions>");
        }
        xml.push_str("</toast>");
        xml
    }
}
type Issued<T> = (Arc<Record<T>>, Vec<Arc<Record<T>>>);

pub(crate) struct Targets<T> {
    pub session: String,
    next: u32,
    pub records: BTreeMap<u32, Arc<Record<T>>>,
}
impl<T> Default for Targets<T> {
    fn default() -> Self {
        Self {
            session: crate::random_hex(16),
            next: 1,
            records: BTreeMap::new(),
        }
    }
}
impl<T> Targets<T> {
    pub fn issue(&mut self, notice: Notice, target: T, replaces: u32) -> Result<Issued<T>> {
        let id = if replaces == 0 {
            let id = self.next;
            self.next = self
                .next
                .checked_add(1)
                .context("notification identifier exhausted")?;
            id
        } else {
            replaces
        };
        let lifetime = if notice.expires > 0 {
            Some(Duration::from_millis(notice.expires as u64))
        } else if notice.transient {
            Some(Duration::from_secs(4))
        } else {
            None
        };
        let record = Arc::new(Record {
            id,
            session: self.session.clone(),
            revision: crate::random_hex(16),
            notice,
            target,
            life: Arc::new(Lifecycle::new(lifetime)),
        });
        let mut retired = Vec::new();
        if let Some(old) = self.records.remove(&id) {
            retired.push(old);
        }
        while self.records.len() >= 128 {
            if let Some((_, old)) = self.records.pop_first() {
                retired.push(old);
            }
        }
        for old in &retired {
            old.life.cancel();
        }
        self.records.insert(id, record.clone());
        Ok((record, retired))
    }
    pub fn remove(&mut self, record: &Arc<Record<T>>) -> bool {
        if self
            .records
            .get(&record.id)
            .is_some_and(|r| Arc::ptr_eq(r, record))
        {
            self.records.remove(&record.id);
            record.life.cancel();
            true
        } else {
            false
        }
    }
    pub fn take(&mut self, argument: &str) -> Result<(Arc<Record<T>>, String)> {
        let parts: Vec<_> = argument.split('-').collect();
        anyhow::ensure!(
            parts.len() == 4 && parts[0] == self.session,
            "notification belongs to another daemon session"
        );
        let id: u32 = parts[2]
            .parse()
            .context("invalid notification identifier")?;
        let record = self
            .records
            .get(&id)
            .context("unknown or expired notification")?;
        anyhow::ensure!(record.life.active(), "notification expired or cancelled");
        anyhow::ensure!(record.revision == parts[1], "notification was replaced");
        let action = parts[3];
        anyhow::ensure!(
            action == "window" || record.notice.actions.iter().any(|(a, _)| *a == action),
            "action not offered by notification"
        );
        let record = self.records.remove(&id).expect("validated notification");
        record.life.cancel();
        Ok((record, action.into()))
    }
}
fn plain(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}
fn escape(value: &str) -> String {
    value
        .chars()
        .map(|c| match c {
            '&' => "&amp;".into(),
            '<' => "&lt;".into(),
            '>' => "&gt;".into(),
            '"' => "&quot;".into(),
            '\'' => "&apos;".into(),
            '\t' | '\n' | '\r' => c.to_string(),
            c if c >= ' ' && c != '\u{fffe}' && c != '\u{ffff}' => c.to_string(),
            _ => "\u{fffd}".into(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn folders_neither_replace_nor_start_file_notification_bursts() -> Result<()> {
        use pneu_proto::{FileBegin, FileKind, Hash, Id, NackCode};
        let mut metadata = FileBegin {
            transfer_id: Id::from_bytes([1; 16]),
            file_name: "report.pdf".into(),
            mime: "application/pdf".into(),
            size: 3,
            sha256: Hash::new("ab".repeat(32))?,
            kind: FileKind::File,
            created_at: 0,
            source_media_id: None,
            hlc: None,
            source_device: None,
            sensitive: false,
            resume: None,
            folder: None,
        };
        let mut targets = Targets::default();
        let (first, _) = targets.issue(Notice::new("report.pdf"), (), 0)?;
        assert!(can_burst(&metadata, None));
        for error in [NackCode::DiskFull, NackCode::NotWritable] {
            assert!(!can_burst(&metadata, Some(error)));
        }

        metadata.folder = Some(Box::new(pneu_proto::Folder { manifest_len: 100 }));
        assert!(!can_burst(&metadata, None));
        let replaces = if can_burst(&metadata, None) {
            first.id
        } else {
            0
        };
        let mut notice = Notice::new("Trip");
        notice.actions = vec![("default", "Open"), ("open", "Open")];
        let (folder, retired) = targets.issue(notice, (), replaces)?;
        assert!(retired.is_empty());
        assert_ne!(folder.id, first.id);
        assert!(first.life.active());

        // A folder also cannot become the candidate replaced by the next file.
        let candidate = can_burst(&metadata, None).then_some(folder.id);
        metadata.folder = None;
        assert!(can_burst(&metadata, None));
        let (next, retired) = targets.issue(Notice::new("next.pdf"), (), candidate.unwrap_or(0))?;
        assert!(retired.is_empty());
        assert_ne!(next.id, folder.id);
        assert!(targets.take(&folder.argument("open")).is_ok());
        assert!(first.life.active());
        Ok(())
    }

    #[tokio::test(start_paused = true)]
    async fn rejects_wrong_session_unknown_expired_replaced_and_unoffered_actions() -> Result<()> {
        let mut targets = Targets::default();
        let (first, _) = targets.issue(Notice::send_failed("file", "peer", "failed"), (), 0)?;
        assert!(
            targets
                .take(&first.argument("retry").replacen(&first.session, "old", 1))
                .is_err()
        );
        assert!(
            targets
                .take(&first.argument("retry").replace("-1-", "-999-"))
                .is_err()
        );
        assert!(targets.take(&first.argument("open")).is_err());
        let (replacement, retired) = targets.issue(first.notice.clone(), (), first.id)?;
        assert_eq!(retired.len(), 1);
        assert!(!first.life.active());
        assert!(!targets.remove(&first));
        assert!(targets.take(&first.argument("retry")).is_err());
        assert!(targets.take(&replacement.argument("retry")).is_ok());
        assert!(targets.take(&replacement.argument("retry")).is_err());
        let (transient, _) = targets.issue(Notice::text("https://example.com"), (), 0)?;
        tokio::time::advance(Duration::from_secs(4)).await;
        assert!(targets.take(&transient.argument("link")).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn capacity_withdraws_oldest_and_keeps_replacement_revision() -> Result<()> {
        let mut targets = Targets::default();
        let (first, _) = targets.issue(Notice::new("first"), (), 0)?;
        for _ in 0..128 {
            targets.issue(Notice::new("next"), (), 0)?;
        }
        assert_eq!(targets.records.len(), 128);
        assert!(!first.life.active());
        assert!(targets.take(&first.argument("window")).is_err());
        Ok(())
    }
    #[tokio::test]
    async fn xml_escapes_text_once_and_arguments_never_contain_targets() -> Result<()> {
        let mut targets = Targets::default();
        let mut n = Notice::new("a<&\"'\u{1}.txt");
        n.body = super::super::notifications::escape("<peer> &lt; & \" '");
        n.actions = vec![("default", "Open"), ("open", "Open & <view> \" '")];
        let (r, _) = targets.issue(n, "C:\\private\\file", 0)?;
        let xml = r.xml(None);
        assert_eq!(r.group().len(), 16);
        assert!(xml.contains("a&lt;&amp;&quot;&apos;\u{fffd}.txt"));
        assert!(xml.contains("&lt;peer&gt; &amp;lt; &amp; &quot; &apos;"));
        assert!(xml.contains("Open &amp; &lt;view&gt; &quot; &apos;"));
        assert!(!xml.contains(r.target));
        assert_eq!(xml.matches("<action ").count(), 1);
        assert!(xml.contains(&format!("launch=\"{}\"", r.argument("default"))));
        Ok(())
    }
    #[tokio::test]
    async fn xml_maps_silent_audio_attribution_and_body_actions() -> Result<()> {
        let mut targets = Targets::default();
        let (quiet, _) = targets.issue(Notice::text("copied"), (), 0)?;
        assert!(quiet.xml(None).contains(r#"<audio silent="true"/>"#));
        let (photo, _) = targets.issue(Notice::new("Photo"), (), 0)?;
        let xml = photo.xml(Some("a<&\"'.png"));
        assert!(!xml.contains("<audio"));
        assert!(xml.contains(
            r#"<text placement="attribution">a&lt;&amp;&quot;&apos;.png</text></binding>"#
        ));
        assert!(!photo.xml(None).contains("attribution"));
        let (burst, _) = targets.issue(Notice::burst(3, None, false, true), (), 0)?;
        assert!(
            burst
                .xml(None)
                .contains(&format!("launch=\"{}\"", burst.argument("show")))
        );
        assert_eq!(targets.take(&burst.argument("show"))?.1, "show");
        let mut n = Notice::new("file");
        n.actions = vec![("show", "Show in Folder"), ("default", "Open")];
        let (file, _) = targets.issue(n, (), 0)?;
        assert!(
            file.xml(None)
                .contains(&format!("launch=\"{}\"", file.argument("default")))
        );
        Ok(())
    }
    #[tokio::test]
    async fn maps_every_notice_kind_to_toast_content_actions_and_lifetime() -> Result<()> {
        use pneu_proto::{FileBegin, FileKind, Hash, Id, NackCode};
        let mut targets = Targets::default();
        let mut notices = vec![
            Notice::text("https://example.com/a?q=1&b=2"),
            Notice::blocked("peer<&"),
            Notice::paired("peer"),
            Notice::unpaired("peer"),
            Notice::sent("file", &["peer".into()]),
            Notice::send_failed("file", "peer", "failed"),
            Notice::port(1234),
            Notice::burst(3, Some(FileKind::Photo), true, true),
        ];
        for kind in [
            FileKind::File,
            FileKind::Photo,
            FileKind::Screenshot,
            FileKind::Recording,
            FileKind::ClipboardImage,
        ] {
            let m = FileBegin {
                resume: None,
                folder: None,
                sensitive: false,
                hlc: None,
                source_device: None,
                transfer_id: Id::from_bytes([0; 16]),
                file_name: "a<&.png".into(),
                mime: "image/png".into(),
                size: 0,
                sha256: Hash::new("0".repeat(64))?,
                kind,
                created_at: 0,
                source_media_id: None,
            };
            for error in [None, Some(NackCode::NotWritable), Some(NackCode::DiskFull)] {
                notices.push(Notice::file(
                    &m,
                    error.is_none(),
                    true,
                    false,
                    error,
                    "C:\\folder",
                    super::super::ReceiveDestination::SaveFolder,
                ));
            }
        }
        for n in notices {
            let (r, _) = targets.issue(n.clone(), (), 0)?;
            let xml = r.xml(None);
            assert!(xml.contains(&format!("<text>{}</text>", escape(&n.summary))));
            assert!(xml.contains(&format!("<text>{}</text>", escape(&plain(&n.body)))));
            assert_eq!(r.life.deadline.is_some(), n.transient || n.expires > 0);
            for (action, label) in n.actions {
                assert!(xml.contains(&r.argument(action)));
                if action != "default" {
                    assert!(xml.contains(&format!("content=\"{}\"", escape(label))));
                }
            }
        }
        Ok(())
    }
}
