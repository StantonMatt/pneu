//! Protocol tests run an existing interpreter, never a freshly written
//! executable (ETXTBSY under concurrent fork/exec). No Wayland connection is made.
use super::*;
use sha2::{Digest, Sha256};
use std::time::Duration;

const HELPER: &str = r#"
import json, os, pathlib, signal, sys, time
root, mode, hashes = pathlib.Path(sys.argv[1]), sys.argv[2], json.loads(sys.argv[3])
def mark(name, data=''):
    (root / name).write_text(str(data))
def wait(name):
    while not (root / name).exists():
        time.sleep(0.005)
def hang():
    while True:
        signal.pause()
mark('pid', os.getpid())
if mode == 'exit_input':
    sys.exit(1)
if mode == 'stall_input':
    hang()
length = int.from_bytes(sys.stdin.buffer.read(8), 'big')
data = sys.stdin.buffer.read(length)
if len(data) != length:
    sys.exit(2)
(root / 'payload').write_bytes(data)
mark('mime', sys.argv[5])
mark('prepared')
if mode == 'stall_ready':
    hang()
if mode == 'partial_ready':
    sys.stdout.write('READY')
    sys.stdout.flush()
    hang()
if mode == 'bad_ready':
    print('READY invalid-hash', flush=True)
    hang()
if mode == 'closed_commit':
    os.close(0)
print('READY ' + ' '.join(hashes), flush=True)
if mode == 'closed_commit':
    hang()
if sys.stdin.buffer.read(1) != b'G':
    sys.exit(3)
mark('committed')
if mode == 'die_commit':
    sys.exit(4)
if mode in ('stall_install', 'controlled'):
    wait('release_install')
mark('installed')
if mode in ('install_no_ack', 'controlled'):
    wait('release_ack')
if mode == 'bad_ack':
    print('WRONG', flush=True)
    hang()
if mode == 'partial_ack':
    sys.stdout.write('INSTALLED')
    sys.stdout.flush()
    hang()
print('INSTALLED', flush=True)
if mode == 'ack_exit':
    sys.exit(0)
if mode == 'ack_crash':
    wait('crash')
    os.kill(os.getpid(), signal.SIGKILL)
wait('exit')
"#;

#[derive(Clone, Copy, Debug)]
enum Payload {
    Text,
    Png,
    MultiMime,
}
impl Payload {
    const ALL: [Self; 3] = [Self::Text, Self::Png, Self::MultiMime];
}
struct FakeHelper {
    root: PathBuf,
    mime: &'static str,
    bytes: Vec<u8>,
    offered: Vec<Vec<u8>>,
    file: Option<ReceivedFile>,
    mode: &'static str,
}
impl FakeHelper {
    fn new(root: &Path, payload: Payload, mode: &'static str) -> Result<Self> {
        std::fs::create_dir(root)?;
        let (mime, bytes) = match payload {
            Payload::Text => (
                "text/plain;charset=utf-8",
                b"candidate remote text".to_vec(),
            ),
            Payload::Png | Payload::MultiMime => {
                let mut bytes = std::io::Cursor::new(Vec::new());
                let (mime, format) = if matches!(payload, Payload::Png) {
                    ("image/png", image::ImageFormat::Png)
                } else {
                    ("image/jpeg", image::ImageFormat::Jpeg)
                };
                // Distinct dimensions ensure a failed image's original and
                // converted hashes cannot accidentally equal the baseline's.
                let width = if root.ends_with("previous") { 2 } else { 3 };
                image::DynamicImage::new_rgb8(width, 3).write_to(&mut bytes, format)?;
                (mime, bytes.into_inner())
            }
        };
        let offered = clipboard_sources(mime.into(), bytes.clone())?
            .into_iter()
            .filter_map(|source| match source {
                wl_clipboard_rs::copy::MimeSource {
                    source: wl_clipboard_rs::copy::Source::Bytes(bytes),
                    mime_type: wl_clipboard_rs::copy::MimeType::Specific(mime),
                } if mime != crate::ui::clipboard::ORIGIN_MIME => Some(bytes.into_vec()),
                _ => None,
            })
            .collect();
        let file = if matches!(payload, Payload::Text) {
            None
        } else {
            let path = root.join("received");
            std::fs::write(&path, &bytes)?;
            Some(ReceivedFile::open(&path)?)
        };
        Ok(Self {
            root: root.to_owned(),
            mime,
            bytes,
            offered,
            file,
            mode,
        })
    }
    fn configure(&self, actions: &mut DesktopActions) {
        let hashes: Vec<_> = self
            .offered
            .iter()
            .map(|bytes| hex::encode(Sha256::digest(bytes)))
            .collect();
        actions.helper = Some((
            "/usr/bin/python3".into(),
            vec![
                "-u".into(),
                "-c".into(),
                HELPER.into(),
                self.root.to_str().unwrap().into(),
                self.mode.into(),
                serde_json::to_string(&hashes).unwrap(),
            ],
        ));
    }
    async fn wait(&self, name: &str) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.root.join(name).exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        Ok(())
    }
    fn pid(&self) -> Result<i32> {
        Ok(std::fs::read_to_string(self.root.join("pid"))?.parse()?)
    }
    fn alive(&self) -> bool {
        self.pid()
            .is_ok_and(|pid| unsafe { libc::kill(pid, 0) } == 0)
    }
    async fn reaped(&self) -> Result<()> {
        self.wait("pid").await?;
        tokio::time::timeout(Duration::from_secs(10), async {
            while self.alive() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        Ok(())
    }
    fn assert_input(&self) -> Result<()> {
        assert_eq!(std::fs::read(self.root.join("payload"))?, self.bytes);
        assert_eq!(std::fs::read_to_string(self.root.join("mime"))?, self.mime);
        Ok(())
    }
}

impl FakeHelper {
    fn input(&self) -> ClipboardInput {
        if let Some(file) = &self.file {
            ClipboardInput::Image(file.clone(), self.mime.into())
        } else {
            ClipboardInput::Text(String::from_utf8(self.bytes.clone()).unwrap())
        }
    }
}
#[tokio::test]
async fn clipboard_prepare_is_inert_and_returns_every_offered_fingerprint() -> Result<()> {
    for payload in Payload::ALL {
        let dir = crate::test_support::scratch()?;
        let helper = FakeHelper::new(&dir.path().join("helper"), payload, "controlled")?;
        let mut actions = DesktopActions::new(dir.path().into());
        helper.configure(&mut actions);
        let mut candidate = actions.prepare_clipboard(helper.input(), 17).await?;
        helper.wait("prepared").await?;
        helper.assert_input()?;
        assert!(!helper.root.join("committed").exists());
        assert!(candidate.marker_confirmation);
        for bytes in &helper.offered {
            assert!(candidate.hashes.contains(&Sha256::digest(bytes).into()));
        }
        actions.install_clipboard(&mut candidate).await?;
        helper.wait("committed").await?;
        // No helper-side INSTALLED acknowledgement is required. Only the
        // daemon watcher can witness installation of this ticket.
        assert!(!helper.root.join("installed").exists());
        candidate.stop().await;
        helper.reaped().await?;
        actions.shutdown().await?;
    }
    Ok(())
}
#[tokio::test]
async fn clipboard_prepare_failures_are_bounded_and_reaped() -> Result<()> {
    for mode in ["exit_input", "stall_ready", "partial_ready", "bad_ready"] {
        let dir = crate::test_support::scratch()?;
        let helper = FakeHelper::new(&dir.path().join("helper"), Payload::Text, mode)?;
        let mut actions = DesktopActions::new(dir.path().into());
        helper.configure(&mut actions);
        actions.helper_timeout = Duration::from_millis(200);
        assert!(
            actions.prepare_clipboard(helper.input(), 1).await.is_err(),
            "{mode}"
        );
        helper.reaped().await?;
        actions.shutdown().await?;
        assert_eq!(actions.jobs.active(), 0);
    }
    Ok(())
}
#[tokio::test]
async fn clipboard_cancelled_preparation_reaps_owned_process() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let helper = FakeHelper::new(&dir.path().join("helper"), Payload::Text, "stall_ready")?;
    let mut actions = DesktopActions::new(dir.path().into());
    helper.configure(&mut actions);
    let actions = Arc::new(actions);
    let input = helper.input();
    let worker = actions.clone();
    let task = tokio::spawn(async move { worker.prepare_clipboard(input, 1).await });
    helper.wait("prepared").await?;
    task.abort();
    let _ = task.await;
    actions.drain_clipboard().await;
    helper.reaped().await?;
    assert_eq!(actions.jobs.active(), 0);
    actions.shutdown().await
}
#[tokio::test]
async fn clipboard_commit_failure_keeps_candidate_available_for_explicit_stop() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let helper = FakeHelper::new(&dir.path().join("helper"), Payload::Text, "closed_commit")?;
    let mut actions = DesktopActions::new(dir.path().into());
    helper.configure(&mut actions);
    let mut candidate = actions.prepare_clipboard(helper.input(), 1).await?;
    assert!(actions.install_clipboard(&mut candidate).await.is_err());
    candidate.stop().await;
    helper.reaped().await?;
    actions.shutdown().await
}
#[tokio::test]
async fn clipboard_helper_that_never_reads_input_is_bounded_and_reaped() -> Result<()> {
    let dir = crate::test_support::scratch()?;
    let mut helper = FakeHelper::new(&dir.path().join("helper"), Payload::Text, "stall_input")?;
    helper.bytes = vec![b'x'; 1024 * 1024];
    let mut actions = DesktopActions::new(dir.path().into());
    helper.configure(&mut actions);
    actions.helper_timeout = Duration::from_millis(200);
    assert!(actions.prepare_clipboard(helper.input(), 1).await.is_err());
    helper.reaped().await?;
    actions.shutdown().await
}
