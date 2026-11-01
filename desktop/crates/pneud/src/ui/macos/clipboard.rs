//! NSPasteboard adapter for the existing clipboard actor. AppKit owns installed
//! bytes, so there is no serving helper to keep alive. Only the watcher confirms
//! install tickets; writeObjects success is not an actor completion witness.
use anyhow::{Result, ensure};
use objc2::{rc::autoreleasepool, runtime::ProtocolObject};
use objc2_app_kit::{NSPasteboard, NSPasteboardItem, NSPasteboardWriting};
use objc2_foundation::{NSArray, NSData, NSString};
use sha2::{Digest, Sha256};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

pub const ORIGIN_MIME: &str = "io.github.stantonmatt.pneu.origin";
pub const MAX_IMAGE: usize = 20 * 1024 * 1024;
const TEXT: &str = "public.utf8-plain-text";
const IMAGE_TYPES: [(&str, &str); 5] = [
    ("public.png", "image/png"),
    ("public.jpeg", "image/jpeg"),
    ("com.compuserve.gif", "image/gif"),
    ("org.webmproject.webp", "image/webp"),
    ("com.microsoft.bmp", "image/bmp"),
];
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Content {
    Text(String),
    Image(Vec<u8>),
    Formats(String, Vec<u8>),
}

#[derive(Debug, PartialEq, Eq)]
enum Classification {
    Denied,
    Sensitive,
    Origin,
    Local,
}
fn classify(access: isize, types: &[String]) -> Classification {
    if access == 3 {
        return Classification::Denied;
    }
    if types.iter().any(|t| {
        matches!(
            t.as_str(),
            "org.nspasteboard.ConcealedType" | "org.nspasteboard.TransientType"
        ) || sensitive_mime(t)
    }) {
        return Classification::Sensitive;
    }
    if types.iter().any(|t| t == ORIGIN_MIME) {
        return Classification::Origin;
    }
    Classification::Local
}
pub fn sensitive_mime(mime: &str) -> bool {
    let mime = mime.to_ascii_lowercase();
    mime.contains("password")
        || mime.contains("secret")
        || mime.contains("keepass")
        || mime.contains("concealed")
        || mime.contains("sensitive")
        || mime == "org.nspasteboard.transienttype"
}
use super::pasteboard_reader::{Baseline, Change, Completion, Phase, ReadResult, ticket};
fn text(bytes: Vec<u8>) -> Option<String> {
    if bytes.len() > crate::store::history::MAX_TEXT {
        return None;
    }
    String::from_utf8(bytes).ok().filter(|s| !s.contains('\0'))
}
fn combine(text: Option<String>, png: Option<Vec<u8>>) -> Option<Content> {
    match (text, png) {
        (Some(t), Some(p)) => Some(Content::Formats(t, p)),
        (Some(t), None) => Some(Content::Text(t)),
        (None, Some(p)) => Some(Content::Image(p)),
        _ => None,
    }
}
/// Prepared, owned representations, with exactly the hashes the watcher reads.
/// Construction never accesses NSPasteboard and is safe to cancel at any time.
pub(crate) struct Offer {
    representations: Vec<(String, Vec<u8>)>,
    ticket: u128,
}
impl Offer {
    #[cfg(test)]
    pub(crate) fn prepare(input: &crate::actions::ClipboardInput, ticket: u128) -> Result<Self> {
        Self::prepare_sensitive(input, ticket, false)
    }
    pub(crate) fn prepare_sensitive(
        input: &crate::actions::ClipboardInput,
        ticket: u128,
        sensitive: bool,
    ) -> Result<Self> {
        use crate::actions::ClipboardInput;
        let mut representations = Vec::new();
        match input {
            ClipboardInput::Text(t) => {
                ensure!(
                    text(t.as_bytes().to_vec()).is_some(),
                    "invalid clipboard text"
                );
                representations.push((TEXT.into(), t.as_bytes().to_vec()));
            }
            ClipboardInput::Formats { text: t, png, .. } => {
                ensure!(
                    text(t.as_bytes().to_vec()).is_some(),
                    "invalid clipboard text"
                );
                representations.push((TEXT.into(), t.as_bytes().to_vec()));
                add_image(&mut representations, "image/png", png.clone())?;
            }
            ClipboardInput::Image(file, mime) => add_image(
                &mut representations,
                mime,
                file.read_bounded(crate::store::history::MAX_IMAGE)?,
            )?,
            ClipboardInput::Bytes { bytes, mime, .. } => {
                add_image(&mut representations, mime, bytes.clone())?
            }
        }
        if sensitive {
            representations.push(("org.nspasteboard.ConcealedType".into(), Vec::new()));
        }
        Ok(Self {
            representations,
            ticket,
        })
    }
    pub(crate) fn hashes(&self) -> Vec<[u8; 32]> {
        self.representations
            .iter()
            .map(|(_, bytes)| Sha256::digest(bytes).into())
            .collect()
    }
    pub(crate) fn install(&self) -> Result<()> {
        self.install_with(|| {})
    }
    fn install_with(&self, after_clear: impl FnOnce()) -> Result<()> {
        autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            ensure!(
                pb.accessBehavior().0 != 3,
                "clipboard access not allowed (macOS AlwaysDeny)"
            );
            let item = NSPasteboardItem::new();
            for (uti, bytes) in &self.representations {
                ensure!(
                    item.setData_forType(&NSData::with_bytes(bytes), &NSString::from_str(uti)),
                    "cannot prepare pasteboard type"
                );
            }
            ensure!(
                item.setData_forType(
                    &NSData::with_bytes(format!("{:032x}", self.ticket).as_bytes()),
                    &NSString::from_str(ORIGIN_MIME)
                ),
                "cannot prepare pasteboard origin"
            );
            // Polling must never submit the intermediate empty selection.
            let _publication = super::pasteboard_reader::PUBLICATION.lock().unwrap();
            pb.clearContents();
            after_clear();
            let writing = ProtocolObject::<dyn NSPasteboardWriting>::from_ref(&*item);
            ensure!(
                pb.writeObjects(&NSArray::from_slice(&[writing])),
                "clipboard access not allowed or pasteboard write rejected"
            );
            Ok(())
        })
    }
}
fn add_image(types: &mut Vec<(String, Vec<u8>)>, mime: &str, bytes: Vec<u8>) -> Result<()> {
    let uti = IMAGE_TYPES
        .iter()
        .find(|(_, m)| *m == mime)
        .map(|(u, _)| *u)
        .ok_or_else(|| anyhow::anyhow!("unsupported clipboard image"))?;
    ensure!(
        bytes.len() <= crate::store::history::MAX_IMAGE,
        "clipboard image too large"
    );
    let decoded = crate::actions::decode_image(&bytes)?;
    if mime != "image/png" {
        let mut png = std::io::Cursor::new(Vec::new());
        decoded.write_to(&mut png, image::ImageFormat::Png)?;
        ensure!(
            png.get_ref().len() <= 32 * 1024 * 1024,
            "converted clipboard image too large"
        );
        types.push(("public.png".into(), png.into_inner()));
    }
    types.push((uti.into(), bytes));
    Ok(())
}

pub struct Watch {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Watch {
    pub fn start(core: Arc<crate::Core>) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let weak = Arc::downgrade(&core);
        let thread = std::thread::Builder::new()
            .name("pneu-pasteboard".into())
            .spawn(move || {
                let mut baseline = Baseline::default();
                let mut policy = super::pasteboard_reader::Policy::default();
                let mut pending: Option<Change<Content>> = None;
                // Include startup/recovery in the same publication-locked
                // snapshots as normal polling; an active ticket may already
                // have been installed before this thread started.
                while !stopping.load(Ordering::SeqCst) {
                    let Some(core) = weak.upgrade() else {
                        break;
                    };
                    autoreleasepool(|_| poll(&core, &mut baseline, &mut policy, &mut pending));
                    drop(core);
                    std::thread::park_timeout(Duration::from_millis(25));
                }
                // Dropping the change kills/reaps its subprocess and retires its
                // actor observation, even if a provider never returns.
                drop(pending);
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
    pub async fn shutdown(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}
impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = &self.thread {
            t.thread().unpark();
        }
    }
}
fn invalidate(core: &crate::Core) {
    let (_, token) = core.observe_watched_clipboard_kind(false, false, false);
    core.clipboard_observed(token, None, false);
}
fn read_command(count: isize, phase: Phase) -> Result<std::process::Command> {
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command
        .arg("pasteboard-read")
        .arg(count.to_string())
        .arg(if phase != Phase::Payload {
            "marker"
        } else {
            "payload"
        });
    Ok(command)
}
fn observe_change(
    core: &Arc<crate::Core>,
    count: isize,
    marker: bool,
    initial: bool,
) -> Change<Content> {
    // Observe exactly once, before the marker subprocess as well as the payload
    // subprocess. The actor retains the frozen recipients under this token.
    let (_, token) = core.observe_watched_clipboard_metadata(false, false, initial, marker);
    let canceled = Arc::new(AtomicBool::new(false));
    core.register_clipboard_read(token, canceled.clone());
    let weak = Arc::downgrade(core);
    Change::new(
        count,
        token,
        canceled,
        if initial {
            Phase::InitialMarker
        } else if marker {
            Phase::Marker
        } else {
            Phase::Payload
        },
        Duration::from_secs(5),
        move |token, completion| {
            let Some(core) = weak.upgrade() else { return };
            match completion {
                Completion::Read(result) => {
                    core.clipboard_observed_hashes(token, result.content, result.fingerprints, true)
                }
                Completion::Unavailable => core.clipboard_observed(token, None, false),
                Completion::Retired => core.retire_clipboard_observation(token),
            }
        },
    )
}
fn poll(
    core: &Arc<crate::Core>,
    baseline: &mut Baseline,
    policy: &mut super::pasteboard_reader::Policy,
    pending: &mut Option<Change<Content>>,
) {
    let pb = NSPasteboard::generalPasteboard();
    let (count, access, types) = {
        let _publication = super::pasteboard_reader::PUBLICATION.lock().unwrap();
        let count = pb.changeCount();
        let access = pb.accessBehavior().0;
        let types = if baseline.changed(count) && access != 3 {
            pb.types()
                .map(|a| a.iter().map(|s| s.to_string()).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        (count, access, types)
    };
    // Always update policy, including unchanged selections and marker branches.
    if policy.update(access) {
        invalidate(core);
    }
    let snapshot = baseline.snapshot(count, access == 3);
    if access == 3 {
        if policy.log_failure() {
            eprintln!("pneud: clipboard access not allowed (macOS AlwaysDeny)");
        }
        pending.take();
        return;
    }
    if let Some(initial) = snapshot {
        let class = classify(access, &types);
        let marker = types.iter().any(|t| t == ORIGIN_MIME);
        if class == Classification::Sensitive && !marker {
            pending.take();
            invalidate(core);
            return;
        }
        if initial && !marker {
            // Establish the baseline without reading or publishing the
            // pre-existing selection. Only an origin marker can witness a
            // write that raced startup/restart/access recovery.
            pending.take();
            return;
        }
        // Capture the new policy before reaping the previous provider, but never
        // start a second subprocess until the old one has been reaped.
        let change = observe_change(core, count, marker, initial);
        pending.take();
        match change.start(read_command) {
            Ok(read) => *pending = Some(read),
            Err(error) => {
                eprintln!("pneud: cannot start pasteboard reader: {error}");
            }
        }
        return;
    }
    if let Some(change) = pending.take() {
        match change.poll(
            pb.changeCount(),
            |ticket| core.clipboard_install_marker(ticket),
            read_command,
        ) {
            Ok(change) => *pending = change,
            Err(error) => {
                if policy.log_failure() {
                    eprintln!("pneud: pasteboard read unavailable: {error}");
                }
            }
        }
    }
}

/// Internal one-shot subprocess. It never starts Core or accesses daemon data.
/// All provider calls, decoding and conversion are killable as one process.
pub fn read_helper(count: isize, marker: bool) -> Result<()> {
    let result = autoreleasepool(|_| read_snapshot(count, marker));
    serde_json::to_writer(std::io::stdout().lock(), &result)?;
    Ok(())
}
fn read_snapshot(count: isize, marker: bool) -> ReadResult<Content> {
    let empty = || ReadResult {
        marker: None,
        content: None,
        fingerprints: Vec::new(),
    };
    let pb = NSPasteboard::generalPasteboard();
    if pb.changeCount() != count || pb.accessBehavior().0 == 3 {
        return empty();
    }
    let types: Vec<String> = pb
        .types()
        .map(|a| a.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default();
    let class = classify(pb.accessBehavior().0, &types);
    // A marker-only read can confirm our sensitive write without reading its payload.
    if class == Classification::Denied || (!marker && class == Classification::Sensitive) {
        return empty();
    }
    if marker {
        let value = pb
            .dataForType(&NSString::from_str(ORIGIN_MIME))
            .filter(|d| d.len() == 32)
            .and_then(|d| ticket(&d.to_vec()))
            .map(|t| format!("{t:032x}"));
        return if pb.changeCount() == count {
            ReadResult {
                marker: value,
                ..empty()
            }
        } else {
            empty()
        };
    }
    let mut fingerprints = Vec::new();
    let mut value = None;
    let mut png = None;
    // Default/Ask are allowed one read per new changeCount. Nil reads are a
    // stable denied/unavailable state until another human copy; never a retry
    // or prompt loop. Do not override the user's system policy.
    if types.iter().any(|t| t == TEXT)
        && let Some(data) = pb
            .dataForType(&NSString::from_str(TEXT))
            .filter(|d| d.len() <= crate::store::history::MAX_TEXT)
    {
        let bytes = data.to_vec();
        value = text(bytes.clone());
        if value.is_some() {
            fingerprints.push(Sha256::digest(&bytes).into());
        }
    }
    for (uti, mime) in IMAGE_TYPES {
        if !types.iter().any(|t| t == uti) {
            continue;
        }
        let cap = if mime == "image/png" {
            32 * 1024 * 1024
        } else {
            crate::store::history::MAX_IMAGE
        };
        if let Some(data) = pb
            .dataForType(&NSString::from_str(uti))
            .filter(|d| d.len() <= cap)
        {
            let bytes = data.to_vec();
            if let Ok(decoded) = crate::actions::decode_image(&bytes) {
                fingerprints.push(Sha256::digest(&bytes).into());
                if png.is_none() {
                    if mime == "image/png" {
                        png = (bytes.len() <= MAX_IMAGE).then_some(bytes);
                    } else {
                        let mut converted = std::io::Cursor::new(Vec::new());
                        if decoded
                            .write_to(&mut converted, image::ImageFormat::Png)
                            .is_ok()
                            && converted.get_ref().len() <= MAX_IMAGE
                        {
                            png = Some(converted.into_inner());
                        }
                    }
                }
            }
        }
    }

    if pb.changeCount() != count {
        return empty();
    }
    ReadResult {
        marker: None,
        content: combine(value, png),
        fingerprints,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::pasteboard_reader::{MarkerAction, marker_action};
    #[tokio::test]
    async fn sensitive_received_offer_keeps_payload_and_requires_marker_confirmation() -> Result<()>
    {
        use crate::actions::PcActions;
        let actions = crate::actions::DesktopActions::new("/unused".into());
        let prepared = actions
            .prepare_clipboard_sensitive(
                crate::actions::ClipboardInput::Text("received secret".into()),
                1,
                true,
            )
            .await?;
        assert!(
            prepared.marker_confirmation,
            "sensitive install must await its marker"
        );
        let offer = Offer::prepare_sensitive(
            &crate::actions::ClipboardInput::Text("received secret".into()),
            1,
            true,
        )?;
        assert!(
            offer
                .representations
                .iter()
                .any(|(name, data)| name == TEXT && data == b"received secret")
        );
        assert!(
            offer
                .representations
                .iter()
                .any(|(name, _)| name == "org.nspasteboard.ConcealedType")
        );
        let names = offer
            .representations
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        assert_eq!(classify(0, &names), Classification::Sensitive);
        Ok(())
    }
    #[test]
    fn classification_respects_privacy_origin_and_unknown_types() {
        for policy in [0, 1, 2] {
            assert_eq!(classify(policy, &[TEXT.into()]), Classification::Local);
            assert_eq!(
                classify(policy, &[ORIGIN_MIME.into(), TEXT.into()]),
                Classification::Origin
            );
            for hint in [
                "org.nspasteboard.ConcealedType",
                "org.nspasteboard.TransientType",
                "application/x-keepassxc-secret",
            ] {
                assert_eq!(
                    classify(policy, &[hint.into(), TEXT.into(), ORIGIN_MIME.into()]),
                    Classification::Sensitive
                );
            }
        }
        assert_eq!(classify(3, &[TEXT.into()]), Classification::Denied);
        assert_eq!(ticket(b"0000000000000000000000000000002a"), Some(42));
        assert_eq!(ticket(b"42"), None);
        assert!(text(vec![255]).is_none());
        assert!(text(b"a\0b".to_vec()).is_none());
        assert!(text(vec![b'a'; crate::store::history::MAX_TEXT + 1]).is_none());
    }
    #[test]
    fn preparation_preserves_all_representations_without_accessing_pasteboard() -> Result<()> {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(1, 1).write_to(&mut png, image::ImageFormat::Png)?;
        let offer = Offer::prepare(
            &crate::actions::ClipboardInput::Formats {
                text: "pneu-m1: pure fixture".into(),
                png: png.into_inner(),
                cache: "/unused".into(),
            },
            42,
        )?;
        assert_eq!(offer.hashes().len(), 2);
        assert_eq!(offer.representations[0].0, TEXT);
        assert_eq!(offer.representations[1].0, "public.png");
        Ok(())
    }
    #[test]
    fn non_png_offers_keep_original_and_normalized_png_fingerprints() -> Result<()> {
        for (mime, format, uti) in [
            ("image/jpeg", image::ImageFormat::Jpeg, "public.jpeg"),
            ("image/gif", image::ImageFormat::Gif, "com.compuserve.gif"),
            (
                "image/webp",
                image::ImageFormat::WebP,
                "org.webmproject.webp",
            ),
            ("image/bmp", image::ImageFormat::Bmp, "com.microsoft.bmp"),
        ] {
            let mut encoded = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(2, 2).write_to(&mut encoded, format)?;
            let original = encoded.into_inner();
            let offer = Offer::prepare(
                &crate::actions::ClipboardInput::Bytes {
                    bytes: original.clone(),
                    mime: mime.into(),
                    cache: "/unused".into(),
                },
                99,
            )?;
            assert_eq!(offer.representations.len(), 2);
            assert_eq!(offer.representations[0].0, "public.png");
            assert_eq!(offer.representations[1], (uti.to_owned(), original.clone()));
            let hashes = offer.hashes();
            assert!(hashes.contains(&Sha256::digest(&original).into()));
            assert!(hashes.contains(&Sha256::digest(&offer.representations[0].1).into()));
            assert_eq!(
                crate::actions::decode_image(&offer.representations[0].1)?.width(),
                2
            );
        }
        Ok(())
    }
    #[test]
    #[ignore = "opt-in: briefly writes the real macOS pasteboard, restores all readable types"]
    fn real_pasteboard_roundtrip_restores_contents() -> Result<()> {
        autoreleasepool(|_| {
            let pb = NSPasteboard::generalPasteboard();
            ensure!(
                pb.accessBehavior().0 == 2,
                "requires AlwaysAllow to avoid permission prompts"
            );
            let mut saved = Vec::new();
            for item in pb.pasteboardItems().iter().flat_map(|a| a.iter()) {
                let mut types = Vec::new();
                for ty in item.types().iter() {
                    let data = item
                        .dataForType(&ty)
                        .ok_or_else(|| anyhow::anyhow!("cannot preserve pasteboard type {ty}"))?;
                    types.push((ty.to_string(), data.to_vec()));
                }
                saved.push(types);
            }
            struct Restore(Vec<Vec<(String, Vec<u8>)>>);
            impl Drop for Restore {
                fn drop(&mut self) {
                    let pb = NSPasteboard::generalPasteboard();
                    let items: Vec<_> = self
                        .0
                        .iter()
                        .map(|types| {
                            let item = NSPasteboardItem::new();
                            for (ty, bytes) in types {
                                assert!(item.setData_forType(
                                    &NSData::with_bytes(bytes),
                                    &NSString::from_str(ty)
                                ));
                            }
                            item
                        })
                        .collect();
                    let objects: Vec<_> = items
                        .iter()
                        .map(|i| ProtocolObject::<dyn NSPasteboardWriting>::from_ref(&**i))
                        .collect();
                    pb.clearContents();
                    if !objects.is_empty() {
                        assert!(pb.writeObjects(&NSArray::from_slice(&objects)));
                    }
                }
            }
            let expected_restore = saved.clone();
            let restore = Restore(saved);
            let mut png = std::io::Cursor::new(Vec::new());
            image::DynamicImage::new_rgb8(1, 1).write_to(&mut png, image::ImageFormat::Png)?;
            let png = png.into_inner();
            let offer = Offer::prepare(
                &crate::actions::ClipboardInput::Formats {
                    text: "pneu-m1: opt-in synthetic pasteboard test".into(),
                    png: png.clone(),
                    cache: "/unused".into(),
                },
                42,
            )?;
            offer.install()?;
            assert_eq!(pb.pasteboardItems().map(|items| items.len()), Some(1));
            assert_eq!(
                pb.dataForType(&NSString::from_str("public.png"))
                    .map(|d| d.to_vec()),
                Some(png)
            );
            assert_eq!(
                pb.dataForType(&NSString::from_str(ORIGIN_MIME))
                    .and_then(|d| ticket(&d.to_vec())),
                Some(42)
            );
            assert_eq!(
                pb.stringForType(&NSString::from_str(TEXT))
                    .map(|s| s.to_string()),
                Some("pneu-m1: opt-in synthetic pasteboard test".into())
            );
            // Exercise the actual clear/write gap while a watcher snapshot is
            // waiting for the same lock used by poll().
            let (cleared_tx, cleared_rx) = std::sync::mpsc::channel();
            let (resume_tx, resume_rx) = std::sync::mpsc::channel();
            let writer = std::thread::spawn(move || {
                offer.install_with(|| {
                    cleared_tx.send(()).unwrap();
                    resume_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                })
            });
            cleared_rx.recv_timeout(Duration::from_secs(2))?;
            assert!(
                super::super::pasteboard_reader::PUBLICATION
                    .try_lock()
                    .is_err()
            );
            resume_tx.send(())?;
            writer.join().unwrap()?;
            let mut baseline = Baseline::default();
            let count = {
                let _publication = super::super::pasteboard_reader::PUBLICATION.lock().unwrap();
                let count = pb.changeCount();
                // Start/restart the watcher after publication completed. Its
                // very first count must still admit the marker, even though
                // ordinary pre-existing content is only a metadata baseline.
                assert!(baseline.changed(count));
                assert_eq!(baseline.snapshot(count, false), Some(true));
                assert!(
                    pb.types()
                        .is_some_and(|types| types.iter().any(|ty| ty.to_string() == ORIGIN_MIME))
                );
                count
            };
            let marker = read_snapshot(count, true);
            assert_eq!(
                marker_action(marker.marker.as_deref().map(str::as_bytes), |t| t == 42),
                MarkerAction::Confirmed
            );
            assert_eq!(baseline.snapshot(count, false), None);
            // Access recovery uses the initial-marker path even if the denied
            // poll already saw the installed count.
            assert_eq!(baseline.snapshot(count, true), None);
            assert_eq!(baseline.snapshot(count, false), Some(true));
            assert_eq!(
                marker_action(
                    read_snapshot(count, true)
                        .marker
                        .as_deref()
                        .map(str::as_bytes),
                    |t| t == 42
                ),
                MarkerAction::Confirmed
            );
            assert_eq!(
                read_snapshot(count, true).marker.as_deref(),
                Some("0000000000000000000000000000002a")
            );
            assert!(read_snapshot(count, false).content.is_some());
            for marker in [
                b"".as_slice(),
                b"short",
                b"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz",
            ] {
                let item = NSPasteboardItem::new();
                assert!(item.setData_forType(
                    &NSData::with_bytes(marker),
                    &NSString::from_str(ORIGIN_MIME)
                ));
                pb.clearContents();
                let writing = ProtocolObject::<dyn NSPasteboardWriting>::from_ref(&*item);
                assert!(pb.writeObjects(&NSArray::from_slice(&[writing])));
                let read = read_snapshot(pb.changeCount(), true);
                assert_eq!(
                    marker_action(read.marker.as_deref().map(str::as_bytes), |_| true),
                    MarkerAction::Invalidate
                );
            }
            drop(restore);
            let actual: Vec<Vec<(String, Vec<u8>)>> = pb
                .pasteboardItems()
                .iter()
                .flat_map(|items| items.iter())
                .map(|item| {
                    item.types()
                        .iter()
                        .map(|ty| (ty.to_string(), item.dataForType(&ty).unwrap().to_vec()))
                        .collect()
                })
                .collect();
            assert_eq!(
                actual, expected_restore,
                "pasteboard contents must be restored"
            );
            Ok(())
        })
    }
}
