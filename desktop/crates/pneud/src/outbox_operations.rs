//! Durable, idempotent asynchronous admission, independent of delivery state.
use super::*;
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AdmissionOperation {
    pub operation: Id,
    pub state: String,
    pub accepted: Vec<Value>,
    pub rejected: Vec<Value>,
    pub created_at: u64,
    pub finished_at: Option<u64>,
    #[serde(skip)]
    pub(super) paths: Vec<PathBuf>,
}
impl Outbox {
    pub(super) fn authorized(&self, c: &Connection, peer: &Authority) -> Result<bool> {
        Ok(c.query_row(
            "SELECT EXISTS(SELECT 1 FROM peers WHERE id=? AND generation=? AND fp=?)",
            params![
                peer.device.as_str(),
                peer.generation as i64,
                peer.fingerprint.as_str()
            ],
            |r| r.get(0),
        )?)
    }
    pub(super) fn operation_allows(
        &self,
        c: &Connection,
        operation: &Id,
        peer: &Authority,
    ) -> Result<bool> {
        Ok(c.query_row("SELECT NOT EXISTS(SELECT 1 FROM outbox_operations WHERE id=?) OR EXISTS(SELECT 1 FROM outbox_operations o,json_each(o.recipients) r WHERE o.id=? AND o.state='staging' AND json_extract(r.value,'$.device')=?)",params![operation.as_str(),operation.as_str(),peer.device.as_str()],|r|r.get(0))?)
    }
    pub(super) fn refresh_operations(&self) -> Result<()> {
        self.store.locked(|c| {
            let operations = {
            let mut q=c.prepare("SELECT id,state,accepted,rejected,paths,created_at,finished_at FROM outbox_operations WHERE dismissed=0 ORDER BY rowid")?;
            q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,i64>(5)?,r.get::<_,Option<i64>>(6)?)))?.map(|r| {
                let (id,state,accepted,rejected,paths,created_at,finished_at)=r?;
                Ok(AdmissionOperation {operation:Id::new(id)?,state,created_at:created_at as u64,finished_at:finished_at.map(|t|t as u64),accepted:serde_json::from_str(&accepted)?,rejected:serde_json::from_str(&rejected)?,paths:serde_json::from_str(&paths)?})
            }).collect::<Result<Vec<_>>>()?
            };
            #[cfg(test)]
            if let Some((entered, release)) = self.projection_barrier.lock().unwrap().take() {
                let _ = entered.send(());
                release.recv_timeout(std::time::Duration::from_secs(10))?;
            }
            // Serialize the read and publication with registrations/mutations.
            // Lock order is always DB -> projection caches.
            *self.operations.lock().expect("outbox operations") = operations;
            Ok(())
        })
    }
    pub(crate) async fn operation_status(&self, id: Id) -> Result<Value> {
        self.store.call(move |c| {
            let row=c.query_row("SELECT state,accepted,rejected FROM outbox_operations WHERE id=?",[id.as_str()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?))).optional()?;
            let (state,accepted,rejected)=match row {Some(row)=>row,None=>{
                let known:bool=c.query_row("SELECT EXISTS(SELECT 1 FROM outbox_requests WHERE operation=?)",[id.as_str()],|r|r.get(0))?;
                ensure!(known,"unknown operation");("expired".into(),"[]".into(),"[]".into())
            }};
            Ok(json!({"operation":id,"state":state,"accepted":serde_json::from_str::<Value>(&accepted)?,"rejected":serde_json::from_str::<Value>(&rejected)?}))
        }).await
    }
    #[cfg(test)]
    pub(crate) async fn start_operation(
        &self,
        paths: Vec<PathBuf>,
        recipients: Vec<Authority>,
        rejected: Vec<Value>,
        request_id: Option<String>,
    ) -> Result<(Id, bool)> {
        self.start_operation_originated(
            paths,
            recipients,
            rejected,
            request_id,
            Id::from_bytes([99; 16]),
        )
        .await
    }
    pub(crate) async fn start_operation_originated(
        &self,
        paths: Vec<PathBuf>,
        recipients: Vec<Authority>,
        rejected: Vec<Value>,
        request_id: Option<String>,
        source_device: Id,
    ) -> Result<(Id, bool)> {
        if let Some(request) = &request_id {
            ensure!(
                !request.is_empty() && request.len() <= 128,
                "invalid request_id"
            );
        }
        let this = self.clone();
        self.store.blocking(move |_| {
            let result=this.store.locked(|c| {
                let tx=c.transaction()?;
                if let Some(request)=&request_id && let Some(id)=tx.query_row("SELECT operation FROM outbox_requests WHERE request_id=?",[request],|r|r.get::<_,String>(0)).optional()? {return Ok((Id::new(id)?,false));}
                let active:i64=tx.query_row("SELECT count(*) FROM outbox_operations WHERE state='staging'",[],|r|r.get(0))?;
                ensure!(active<16,"file admission is busy");
                // Freeze each image's ordering before staging can delay it.
                // Distinct images advance in selection order, so later local
                // clipboard activity fences every artifact in this batch.
                let mut stamps = HashMap::new();
                for path in &paths {
                    if mime(path).starts_with("image/") && !stamps.contains_key(path) {
                        let (created_at, hlc) = allocate_stamp(&tx)?;
                        stamps.insert(path.clone(), ArtifactStamp { created_at, hlc, source_device: source_device.clone() });
                    }
                }
                let stamp = (!stamps.is_empty()).then(|| serde_json::to_string(&stamps)).transpose()?;
                let id=Id::new(crate::random_hex(16))?;
                tx.execute("INSERT INTO outbox_operations(id,request_id,state,paths,recipients,rejected,created_at,stamp) VALUES(?,?,'staging',?,?,?,?,?)",params![id.as_str(),request_id,serde_json::to_string(&paths)?,serde_json::to_string(&recipients)?,serde_json::to_string(&rejected)?,crate::now_ms() as i64,stamp])?;
                if let Some(request)=request_id {tx.execute("INSERT INTO outbox_requests(request_id,operation) VALUES(?,?)",params![request,id.as_str()])?;}
                tx.commit()?;
                // The operation is durable. Build the projection without an
                // additional fallible read before handing it to its worker.
                this.operations.lock().expect("outbox operations").push(AdmissionOperation {operation:id.clone(),state:"staging".into(),accepted:Vec::new(),rejected,created_at:crate::now_ms(),finished_at:None,paths});
                Ok((id,true))
            })?;
            Ok(result)
        }).await
    }
    pub(crate) async fn request_operation(&self, request: String) -> Result<Option<Id>> {
        self.store
            .call(move |c| {
                Ok(c.query_row(
                    "SELECT operation FROM outbox_requests WHERE request_id=?",
                    [request],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .map(Id::new)
                .transpose()?)
            })
            .await
    }
    pub(crate) fn admission_cancelled(&self, id: &Id) -> bool {
        self.cancelled_admissions
            .lock()
            .expect("cancelled admissions")
            .contains(id)
    }
    pub(crate) async fn admission_result(
        &self,
        id: Id,
        accepted: Vec<Value>,
        rejected: Vec<Value>,
        done: bool,
    ) -> Result<()> {
        let this = self.clone();
        self.store.blocking(move |_| {
            this.store.locked(|c| {
                c.execute("UPDATE outbox_operations SET accepted=?,rejected=CASE WHEN state='cancelled' THEN rejected ELSE ? END,finished_at=CASE WHEN ? THEN ? ELSE finished_at END,state=CASE WHEN state='cancelled' THEN state WHEN ? THEN CASE WHEN json_array_length(?)=0 THEN 'failed' ELSE 'admitted' END ELSE state END WHERE id=?",params![serde_json::to_string(&accepted)?,serde_json::to_string(&rejected)?,done,crate::now_ms() as i64,done,serde_json::to_string(&accepted)?,id.as_str()])?;Ok(())
            })?;
            this.prune_operations()?;this.refresh_operations()?;this.refresh()?;
            if done { this.cancelled_admissions.lock().expect("cancelled admissions").remove(&id); }
            Ok(())
        }).await
    }
    pub(super) fn prune_operations(&self) -> Result<()> {
        self.store.locked(|c| {c.execute("DELETE FROM outbox_operations WHERE state<>'staging' AND id NOT IN (SELECT id FROM outbox_operations ORDER BY rowid DESC LIMIT 50) AND NOT EXISTS(SELECT 1 FROM outbox_artifacts WHERE operation=outbox_operations.id AND staged=1)",[])?;Ok(())})
    }
    pub(super) fn validate_artifact(
        &self,
        c: &Connection,
        operation: &Id,
        artifact: Option<&Id>,
    ) -> Result<()> {
        if let Some(artifact) = artifact {
            let belongs: bool = c.query_row(
                "SELECT EXISTS(SELECT 1 FROM outbox_artifacts WHERE id=? AND operation=?)",
                params![artifact.as_str(), operation.as_str()],
                |r| r.get(0),
            )?;
            ensure!(belongs, "artifact does not belong to operation");
        }
        Ok(())
    }
    pub(crate) async fn check_artifact(&self, operation: Id, artifact: Option<Id>) -> Result<()> {
        let this = self.clone();
        self.store
            .call(move |c| this.validate_artifact(c, &operation, artifact.as_ref()))
            .await
    }
    pub(crate) async fn dismiss(&self, id: Id, artifact: Option<Id>) -> Result<()> {
        let this = self.clone();
        self.store.blocking(move |_| {
            this.store.locked(|c| {
                let tx = c.transaction()?;
                this.validate_artifact(&tx, &id, artifact.as_ref())?;
                let active:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM outbox_deliveries d JOIN outbox_artifacts a ON a.id=d.artifact WHERE a.operation=? AND (? IS NULL OR a.id=?) AND d.state IN ('sending','queued')) OR (? IS NULL AND EXISTS(SELECT 1 FROM outbox_operations WHERE id=? AND state='staging'))",params![id.as_str(),artifact.as_ref().map(Id::as_str),artifact.as_ref().map(Id::as_str),artifact.as_ref().map(Id::as_str),id.as_str()],|r|r.get(0))?;
                ensure!(!active,"operation is still active");
                tx.execute("INSERT OR IGNORE INTO file_pending_cancels SELECT d.device,d.artifact,d.generation FROM outbox_deliveries d JOIN outbox_artifacts a ON a.id=d.artifact WHERE a.operation=? AND (? IS NULL OR a.id=?) AND d.state NOT IN ('sent','cancelled') AND d.resume=1",params![id.as_str(),artifact.as_ref().map(Id::as_str),artifact.as_ref().map(Id::as_str)])?;
                // A hidden failed delivery must not retain queue capacity forever.
                tx.execute("UPDATE outbox_deliveries SET state='cancelled',error=NULL WHERE artifact IN (SELECT id FROM outbox_artifacts WHERE operation=? AND (? IS NULL OR id=?)) AND state NOT IN ('sent','cancelled')", params![id.as_str(),artifact.as_ref().map(Id::as_str),artifact.as_ref().map(Id::as_str)])?;
                if let Some(artifact) = &artifact {
                    tx.execute("INSERT OR IGNORE INTO outbox_artifact_dismissed(artifact) VALUES(?)",[artifact.as_str()])?;
                } else {
                    tx.execute("INSERT OR IGNORE INTO outbox_dismissed(operation) VALUES(?)",[id.as_str()])?;
                    // Older queued history predates operation records.
                    tx.execute("INSERT INTO outbox_operations(id,state,paths,recipients,dismissed) VALUES(?,'admitted','[]','[]',1) ON CONFLICT(id) DO UPDATE SET dismissed=1",[id.as_str()])?;
                }
                tx.commit()?;
                Ok(())
            })?;this.refresh_operations()?;this.refresh()?;this.collect()
        }).await
    }
    pub(crate) async fn reconcile_sessions(
        &self,
        live: impl FnOnce() -> Vec<(Id, u64, u64)> + Send + 'static,
    ) -> Result<()> {
        let this = self.clone();
        self.store.blocking(move |_| {
            this.store.locked(|c| {
                // Session authority must be read after acquiring the DB lock:
                // a replacement may have claimed while this job was queued.
                let live=live();
                let mut q=c.prepare("SELECT DISTINCT device,generation,session FROM outbox_deliveries WHERE state='sending'")?;
                let rows=q.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
                drop(q);
                for (device,generation,session) in rows {
                    if !live.iter().any(|(d,g,s)|d.as_str()==device && *g==generation as u64 && *s==session as u64) {
                        c.execute("UPDATE outbox_deliveries SET state=?,error='Connection lost',retry_at=0 WHERE device=? AND generation=? AND session=? AND state='sending'",params![this.disconnected_state(),device,generation,session])?;
                    }
                }
                Ok(())
            })?;this.refresh()
        }).await
    }
}
