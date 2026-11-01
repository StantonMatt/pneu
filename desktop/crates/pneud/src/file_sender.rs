//! File admission and the single outbound FILE lane (shared with clipboard images).
use super::*;
use crate::file_recipients::{Selection, resolve};
use crate::outbox::{self, Authority, Delivery, Targets};
use tokio::io::AsyncReadExt;

impl Core {
    pub(super) fn file_targets(
        &self,
        devices: &[mesh::DeviceSnapshot],
        sessions: &Connections,
        admission: &Admission,
    ) -> Vec<outbox::SendTarget> {
        devices
            .iter()
            .map(|d| {
                let generation = self.store.pairing_generation(&d.device);
                let connected = sessions
                    .sessions
                    .get(&d.device)
                    .filter(|s| !s.close.is_closed() && s.close.borrow().is_none());
                let supported = connected
                    .map(|s| s.capabilities.iter().any(|c| c == "files"))
                    .or_else(|| generation.and_then(|g| self.outbox.supports(&d.device, g)))
                    .unwrap_or(d.role == "computer");
                let reason = if generation.is_none()
                    || sessions.unpairing.contains(&d.device)
                    || matches!(
                        d.state.as_str(),
                        "pairing_lost"
                            | "unauthorized"
                            | "binding_mismatch"
                            | "fingerprint_mismatch"
                    ) {
                    Some("Pairing is unavailable")
                } else if !supported {
                    Some("Update this device to receive files")
                } else {
                    None
                };
                outbox::SendTarget {
                    device: d.device.clone(),
                    name: d.name.clone(),
                    generation: generation.unwrap_or_default(),
                    fingerprint: admission
                        .peers
                        .iter()
                        .find(|(_, id)| *id == &d.device)
                        .map(|(fp, _)| fp.clone()),
                    eligible: reason.is_none(),
                    would_queue: reason.is_none() && connected.is_none(),
                    reason: reason.map(str::to_owned),
                }
            })
            .collect()
    }
    /// Untargeted CLI/file-manager sends enter the same picker as the window. The UI
    /// stays open for asynchronous admission failures in Recent.
    pub(crate) fn present_files(
        &self,
        paths: Vec<PathBuf>,
        targets: Targets,
        request_id: Option<String>,
    ) -> Result<serde_json::Value> {
        ensure!(
            !paths.is_empty() && paths.len() <= 128,
            "choose 1 to 128 files"
        );
        let snapshot = self.ui_snapshot();
        let targeted = matches!(targets, Targets::Captured(_));
        let mode = match &targets {
            Targets::Captured(entries) => Selection::Captured(entries),
            Targets::All(value) => {
                ensure!(value == "all", "targets must be all or device IDs");
                let config = self.config();
                Selection::AllEligible {
                    queue_offline: config.choose_devices_each_time || config.queue_offline,
                }
            }
            Targets::Devices(_) => anyhow::bail!("explicit devices must use send_files"),
        };
        let resolution = resolve(&snapshot.send_targets, mode)?;
        if targeted && let Some(rejected) = resolution.rejected.first() {
            anyhow::bail!("{}: {}", rejected.device, rejected.reason);
        }
        let recipients = resolution.recipients;
        self.present_send(crate::ui::window::SendRequest {
            id: request_id.unwrap_or_else(|| crate::random_hex(16)),
            paths: Some(paths),
            targets: recipients,
            clipboard: false,
            targeted,
            error: None,
        })
    }
    pub(crate) fn present_send_error(&self, message: String) -> Result<serde_json::Value> {
        ensure!(message.len() <= 4096, "send error is too long");
        self.present_send(crate::ui::window::SendRequest {
            id: crate::random_hex(16),
            paths: None,
            targets: Vec::new(),
            clipboard: false,
            targeted: false,
            error: Some(message),
        })
    }
    pub(crate) fn present_send(
        &self,
        request: crate::ui::window::SendRequest,
    ) -> Result<serde_json::Value> {
        ensure!(
            !request.id.is_empty() && request.id.len() <= 128,
            "invalid request_id"
        );
        ensure!(request.targets.len() <= 128, "choose up to 128 devices");
        let id = request.id.clone();
        {
            let mut pending = self.pending_sends.lock().expect("pending sends");
            if !pending.iter().any(|r| r.id == id) {
                ensure!(pending.len() < 16, "send window is busy");
                pending.push_back(request);
            }
        }
        self.request_window(false);
        let _ = self.events.send(Event::ConfigChanged);
        Ok(serde_json::json!({"request":id}))
    }
    pub(crate) async fn notify_send_admission_failure(&self, reason: &str) {
        if let Err(error) = self.actions.notify_send_admission_failure(reason).await {
            self.error(
                None,
                format!("send failure notification unavailable: {error}"),
            );
        }
    }
    pub(crate) fn pending_send(&self) -> Option<crate::ui::window::SendRequest> {
        self.pending_sends
            .lock()
            .expect("pending sends")
            .front()
            .cloned()
    }
    pub(crate) fn acknowledge_send(&self, request: &str) {
        let mut pending = self.pending_sends.lock().expect("pending sends");
        if pending.front().is_some_and(|r| r.id == request) {
            pending.pop_front();
        }
    }
    pub async fn send_files(
        self: &Arc<Self>,
        paths: Vec<PathBuf>,
        targets: Targets,
    ) -> Result<serde_json::Value> {
        self.send_files_request(paths, targets, None).await
    }
    pub async fn send_files_request(
        self: &Arc<Self>,
        paths: Vec<PathBuf>,
        targets: Targets,
        request_id: Option<String>,
    ) -> Result<serde_json::Value> {
        ensure!(
            !paths.is_empty() && paths.len() <= 128,
            "choose 1 to 128 files"
        );
        // Registration and staging have task ownership before their caller can
        // disappear. A timed-out retry can always recover the same operation.
        let core = self.clone();
        let tracked = crate::work::track(vec![self.file_admissions.clone()]);
        let (reply, result) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _tracked = tracked;
            let registration = core
                .register_file_operation(paths.clone(), targets, request_id)
                .await;
            let (operation, recipients, mut rejected, new) = match registration {
                Ok(value) => value,
                Err(error) => {
                    let _ = reply.send(Err(error));
                    return;
                }
            };
            let _ = reply.send(Ok(
                serde_json::json!({"operation":operation,"rejected":rejected}),
            ));
            if !new {
                return;
            }
            let mut accepted = Vec::new();
            for path in paths {
                if core.outbox.admission_cancelled(&operation) {
                    break;
                }
                let result = if *core.shutdown.borrow() {
                    Err(anyhow::anyhow!("daemon is stopping"))
                } else {
                    core.outbox
                        .admit_selected(
                            operation.clone(),
                            path.clone(),
                            recipients.clone(),
                            core.config().max_file_bytes,
                            core.identity.device_id.clone(),
                        )
                        .await
                };
                if core.outbox.admission_cancelled(&operation) {
                    // The artifact may have committed just before cancellation.
                    // Preserve its identity while discarding cancellation errors.
                    if let Ok((id, _)) = result {
                        accepted.push(serde_json::json!({"path":path,"transfer_id":id}));
                    }
                    break;
                }
                match result {
                    Ok((id, failures)) => {
                        accepted.push(serde_json::json!({"path":path,"transfer_id":id}));
                        for failure in failures {
                            rejected.push(serde_json::json!({"path":path,"device":failure.device,"reason":failure.reason}));
                        }
                    }
                    Err(error) => {
                        let failure = error.downcast_ref::<outbox::AdmissionFailure>();
                        if let Some(failure) = failure {
                            for recipient in &failure.rejected {
                                rejected.push(serde_json::json!({"path":path,"device":recipient.device,"reason":recipient.reason}));
                            }
                        }
                        if failure.is_none_or(|f| !f.recipient_only || f.rejected.is_empty()) {
                            rejected
                                .push(serde_json::json!({"path":path,"reason":error.to_string()}));
                        }
                    }
                }
                core.persist_admission_result(
                    operation.clone(),
                    accepted.clone(),
                    rejected.clone(),
                    false,
                )
                .await;
                for peer in &recipients {
                    core.wake(&peer.device);
                }
                core.mesh.changed.notify_one();
                let _ = core.events.send(Event::ConfigChanged);
            }
            core.persist_admission_result(operation, accepted, rejected, true)
                .await;
            let _ = core.events.send(Event::ConfigChanged);
        });
        result.await.context("file admission stopped")?
    }
    async fn register_file_operation(
        &self,
        paths: Vec<PathBuf>,
        targets: Targets,
        request_id: Option<String>,
    ) -> Result<(Id, Vec<Authority>, Vec<serde_json::Value>, bool)> {
        // Serialize request lookup with registration so concurrent retries also
        // recover the persisted recipient decisions rather than resolving anew.
        let _guard = self.pairing_gate.lock().await;
        if let Some(request) = &request_id
            && let Some(operation) = self.outbox.request_operation(request.clone()).await?
        {
            let status = self.outbox.operation_status(operation.clone()).await?;
            let rejected = serde_json::from_value(status["rejected"].clone())?;
            return Ok((operation, Vec::new(), rejected, false));
        }
        let snapshot = self.ui_snapshot();
        let mode = match &targets {
            Targets::Captured(entries) => {
                ensure!(!entries.is_empty(), "choose 1 to 128 devices");
                Selection::Captured(entries)
            }
            Targets::Devices(ids) => Selection::Explicit(ids),
            Targets::All(value) => {
                ensure!(value == "all", "targets must be all or device IDs");
                Selection::AllEligible {
                    queue_offline: self.config().queue_offline,
                }
            }
        };
        let resolution = resolve(&snapshot.send_targets, mode)?;
        let recipients = resolution.recipients;
        let rejected: Vec<_> = resolution
            .rejected
            .into_iter()
            .map(|r| serde_json::json!({"device":r.device,"reason":r.reason}))
            .collect();
        let (operation, new) = self
            .outbox
            .start_operation_originated(
                paths,
                recipients.clone(),
                rejected.clone(),
                request_id,
                self.identity.device_id.clone(),
            )
            .await?;
        let _ = self.events.send(Event::ConfigChanged);
        Ok((operation, recipients, rejected, new))
    }
    async fn persist_admission_result(
        &self,
        operation: Id,
        accepted: Vec<serde_json::Value>,
        rejected: Vec<serde_json::Value>,
        done: bool,
    ) {
        loop {
            match self
                .outbox
                .admission_result(operation.clone(), accepted.clone(), rejected.clone(), done)
                .await
            {
                Ok(()) => return,
                Err(error) => {
                    self.error(
                        None,
                        format!("admission result persistence deferred: {error}"),
                    );
                    if *self.shutdown.borrow() {
                        return;
                    }
                    tokio::select! {_=self.wait_shutdown()=>return,_=tokio::time::sleep(Duration::from_millis(250))=>{}}
                }
            }
        }
    }
    pub async fn operation_status(&self, operation: Id) -> Result<serde_json::Value> {
        self.outbox.operation_status(operation).await
    }
    pub async fn dismiss_transfer(&self, operation: Id) -> Result<()> {
        self.dismiss_transfer_artifact(operation, None).await
    }
    pub async fn dismiss_transfer_artifact(
        &self,
        operation: Id,
        artifact: Option<Id>,
    ) -> Result<()> {
        {
            let mut ui = self.ui.lock().expect("UI state");
            if let Some(row) = ui.incoming.iter().find(|t| t.operation == operation) {
                ensure!(
                    artifact.as_ref().is_none_or(|id| id == &row.transfer_id),
                    "artifact does not belong to operation"
                );
                ensure!(row.state != "sending", "operation is still active");
                ui.incoming.retain(|t| t.operation != operation);
                let _ = self.events.send(Event::ConfigChanged);
                return Ok(());
            }
        }
        self.outbox.dismiss(operation, artifact).await?;
        let _ = self.events.send(Event::ConfigChanged);
        Ok(())
    }
    pub async fn cancel_transfer(&self, operation: Id, device: Option<Id>) -> Result<()> {
        self.cancel_transfer_artifact(operation, None, device).await
    }
    pub async fn cancel_transfer_artifact(
        &self,
        operation: Id,
        artifact: Option<Id>,
        device: Option<Id>,
    ) -> Result<()> {
        self.outbox
            .change(operation, artifact, device.clone(), false)
            .await?;
        // A partially emitted file has no wire cancellation frame. Close just
        // the affected lane's connection, so the receiver drops its partial.
        let sessions = self.sessions.lock().expect("sessions");
        for s in sessions
            .sessions
            .values()
            .filter(|s| device.as_ref().is_none_or(|d| d == &s.device))
        {
            s.wake.notify_one();
        }
        let _ = self.events.send(Event::ConfigChanged);
        Ok(())
    }
    pub async fn retry_transfer(&self, operation: Id, device: Id) -> Result<()> {
        self.retry_transfer_artifact(operation, None, device).await
    }
    pub async fn retry_transfer_artifact(
        &self,
        operation: Id,
        artifact: Option<Id>,
        device: Id,
    ) -> Result<()> {
        self.outbox
            .check_artifact(operation.clone(), artifact.clone())
            .await?;
        let sessions = self.sessions.clone();
        let store = self.store.clone();
        self.outbox
            .reconcile_sessions(move || {
                let sessions = sessions.lock().expect("sessions");
                sessions
                    .sessions
                    .values()
                    .filter(|s| !s.close.is_closed() && s.close.borrow().is_none())
                    .filter_map(|s| {
                        store
                            .pairing_generation(&s.device)
                            .map(|g| (s.device.clone(), g, s.id))
                    })
                    .collect()
            })
            .await?;
        let snapshot = self.ui_snapshot();
        let target = snapshot
            .send_targets
            .iter()
            .find(|t| t.device == device)
            .context("device is not paired")?;
        ensure!(
            target.eligible,
            "{}",
            target.reason.as_deref().unwrap_or("device is unavailable")
        );
        self.outbox
            .change(operation, artifact, Some(device.clone()), true)
            .await?;
        self.wake(&device);
        self.mesh.changed.notify_one();
        let _ = self.events.send(Event::ConfigChanged);
        Ok(())
    }
    fn file_current(&self, delivery: &Delivery, connection: u64) -> bool {
        let sessions = self.sessions.lock().expect("sessions");
        !sessions.unpairing.contains(&delivery.authority.device)
            && sessions
                .sessions
                .get(&delivery.authority.device)
                .is_some_and(|s| {
                    s.id == connection
                        && s.capabilities.iter().any(|c| c == "files")
                        && !s.close.is_closed()
                        && s.close.borrow().is_none()
                })
            && self.outbox.current(delivery)
    }
}
pub(super) struct FileWrite {
    pub delivery: Delivery,
    pub connection: u64,
    pub mesh: bool,
    pub file: Option<tokio::fs::File>,
}
/// The first accepted byte must still have the captured binding and capability,
/// even if the socket previously returned Pending. Later cancellation closes
/// the connection, never truncates a frame and appends protocol traffic.
struct FileWriter<'a, W> {
    core: &'a Core,
    writer: &'a mut W,
    work: &'a FileWrite,
}
impl<W: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for FileWriter<'_, W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = &mut *self;
        let sessions = this.core.sessions.lock().expect("sessions");
        let valid = !sessions
            .unpairing
            .contains(&this.work.delivery.authority.device)
            && sessions
                .sessions
                .get(&this.work.delivery.authority.device)
                .is_some_and(|s| {
                    s.id == this.work.connection && s.capabilities.iter().any(|c| c == "files")
                })
            && this.core.outbox.current(&this.work.delivery);
        if !valid {
            return std::task::Poll::Ready(Err(std::io::Error::other("file delivery cancelled")));
        }
        std::pin::Pin::new(&mut *this.writer).poll_write(cx, bytes)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.writer).poll_flush(cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut *self.writer).poll_shutdown(cx)
    }
}
pub(super) async fn transmit_file<W: tokio::io::AsyncWrite + Unpin>(
    core: &Core,
    writer: &mut W,
    mut work: FileWrite,
    priority: &mut mpsc::Receiver<Message>,
    normal: &mut phone_controls::FrameQueue,
    text: &mut mpsc::Receiver<AutomaticWrite>,
    reports: &mpsc::Sender<WriteReport>,
) -> Result<()> {
    let mut file = work.file.take().context("verified file reader")?;
    let mut begin = work.delivery.begin.clone();
    if !work.mesh {
        if let Some(hlc) = begin.hlc {
            begin.created_at = hlc;
        }
        begin.hlc = None;
        begin.source_device = None;
    }
    pneu_proto::encode_frame(&Message::FileBegin(begin.clone()))?;
    let mut writer = FileWriter {
        core,
        writer,
        work: &work,
    };
    send(&mut writer, &Message::FileBegin(begin)).await?;
    let mut offset = 0;
    let mut buffer = vec![0; CHUNK_SIZE];
    while offset < work.delivery.begin.size {
        let n = file.read(&mut buffer).await?;
        ensure!(n > 0, "staged file truncated");
        ensure!(
            core.file_current(&work.delivery, work.connection),
            "file delivery cancelled"
        );
        send_pending_messages(core, &mut *writer.writer, priority, normal, Some(reports)).await?;
        send_pending_clipboard_text(
            core,
            &mut *writer.writer,
            priority,
            normal,
            text,
            Some(reports),
        )
        .await?;
        send(
            &mut writer,
            &Message::FileChunk(FileChunk {
                transfer_id: work.delivery.begin.transfer_id.clone(),
                offset,
                data: buffer[..n].to_vec(),
            }),
        )
        .await?;
        offset += n as u64;
        core.outbox.progress(&work.delivery, offset);
        let _ = core.events.send(Event::ConfigChanged);
    }
    send(
        &mut writer,
        &Message::FileEnd(FileEnd {
            transfer_id: work.delivery.begin.transfer_id.clone(),
        }),
    )
    .await?;
    Ok(())
}

pub(super) struct SessionRecovery {
    outbox: outbox::Outbox,
    device: Id,
    generation: u64,
    session: u64,
}
impl Drop for SessionRecovery {
    fn drop(&mut self) {
        self.outbox.defer_recovery(
            self.device.clone(),
            self.generation,
            self.session,
            Some("Connection interrupted".into()),
        );
    }
}
impl Core {
    pub(super) fn arm_file_recovery(
        &self,
        device: Id,
        generation: u64,
        session: u64,
    ) -> SessionRecovery {
        SessionRecovery {
            outbox: self.outbox.clone(),
            device,
            generation,
            session,
        }
    }
}
