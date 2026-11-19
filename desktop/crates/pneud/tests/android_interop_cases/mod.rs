//! Opt-in real Android client against the production desktop Core/protocol machines.
use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[tokio::test]
#[ignore = "run with android/tools/run_nov2_interop.py on the disposable pneu-test AVD"]
async fn real_android_nov2_extensions() -> Result<()> {
    let ready = std::env::var_os("PNEU_ANDROID_INTEROP_READY").context("interop readiness path")?;
    let ready = PathBuf::from(ready);
    let h = Harness::mesh().await?;
    h.core
        .set_setting("queue_offline".into(), true.into())
        .await?;
    let invitation = h
        .core
        .start_pairing_with_addresses(vec![h.address.replace("127.0.0.1", "10.0.2.2")])
        .await?;
    let control = TcpListener::bind("127.0.0.1:0").await?;
    // A filesystem Unix socket must fit SUN_LEN even when evidence lives under
    // a long agent-scratch path. This directory contains only the tiny socket.
    let ipc_dir = tempfile::tempdir()?;
    let ipc_path = ipc_dir.path().join("p.sock");
    let (listener, _owner) = pneud::ipc::bind(&ipc_path)?;
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let core = h.core.clone();
    let ipc = tokio::spawn(pneud::ipc::serve(core, listener, stopped));
    std::fs::write(
        &ready,
        serde_json::to_vec(
            &serde_json::json!({"uri":invitation.build()?,"controlPort":control.local_addr()?.port()}),
        )?,
    )?;
    let mut checkpoint = 0;
    let mut folder_evidence = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(330), async {
        loop {
            let (stream, _) = control.accept().await?;
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await?;
            anyhow::ensure!(line.len() < 4096, "oversized interop request");
            let request: serde_json::Value = serde_json::from_str(&line)?;
            let action = request["action"].as_str().context("interop action")?;
            let mut done = false;
            let value = match action {
                "status" => serde_json::json!({"snapshot":h.core.ui_snapshot(),
                    "notices":h.actions.phone_notices.lock().unwrap().iter().map(|(id,handle,n)|serde_json::json!({"id":id,"handle":handle,"title":n.title,"text":n.text,"app":n.app})).collect::<Vec<_>>(),
                    "removed":h.actions.phone_removed.lock().unwrap().len()}),
                "ring" => {
                    let device: Id = serde_json::from_value(request["device"].clone())?;
                    let connection = h.core.ui_snapshot().phone_controls[&device].connection.context("ring connection")?;
                    pneud::ipc::request(&ipc_path, pneud::ipc::Command::Ring {
                        device, connection, action: serde_json::from_value(request["ring"].clone())?,
                    }).await?;
                    serde_json::json!({"ok":true})
                }
                "checkpoint" => {
                    let db = rusqlite::Connection::open_with_flags(h.core.paths.data.join("state.sqlite3"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
                    let transfer = request["transfer"].as_str();
                    let offset: i64 = db.query_row("SELECT coalesce(max(json_extract(record,'$.offset')),0) FROM file_partials WHERE (?1 IS NULL OR id=?1)", [transfer], |r|r.get(0))?;
                    if transfer.is_none() { checkpoint = checkpoint.max(offset); }
                    serde_json::json!({"offset":offset})
                }
                "folder_received" => {
                    let large = request["large"].as_bool().context("folder size")?;
                    let mut found = false;
                    let mut count = 0;
                    for entry in std::fs::read_dir(h.core.config().save_path)? {
                        let path = entry?.path();
                        // The save root also contains private staging directories.
                        // Count only this fixture's published destination names.
                        if !path.is_dir() || !path.file_name().is_some_and(|name| name.to_string_lossy().starts_with("Trip")) { continue; }
                        count += 1;
                        let bytes = std::fs::read(path.join("a.txt")).with_context(|| format!("received folder {}", path.display()))?;
                        if bytes.len() != if large { 8 * 1024 * 1024 } else { 3 } { continue; }
                        anyhow::ensure!(path.join("empty").is_dir(), "empty child directory missing");
                        anyhow::ensure!(std::fs::read_dir(&path)?.count() == 2, "unexpected folder child");
                        if large {
                            anyhow::ensure!(hex::encode(Sha256::digest(&bytes)) == request["sha256"].as_str().context("folder content hash")?, "resumed folder content differs");
                            let checkpoint = request["checkpoint"].as_u64().context("folder checkpoint")?;
                            let resumed = request["resumedOffset"].as_u64().context("folder resumed offset")?;
                            anyhow::ensure!(checkpoint >= 4 * 1024 * 1024 && resumed >= checkpoint, "folder did not resume a retained prefix");
                        } else { anyhow::ensure!(bytes == b"abc", "small folder content differs"); }
                        found = true;
                    }
                    anyhow::ensure!(found && count == if large {2} else {1}, "wrong number/content of received folders");
                    let snapshot = h.core.ui_snapshot();
                    anyhow::ensure!(snapshot.send_targets.iter().any(|t| t.folder_reason.as_deref() == Some("Can't receive folders yet")), "phone folder refusal label missing");
                    folder_evidence.push(request.clone());
                    serde_json::json!({"ok":true})
                }
                "done" => {
                    anyhow::ensure!(checkpoint >= 4 * 1024 * 1024, "no retained desktop checkpoint");
                    let files = h.saved()?;
                    anyhow::ensure!(files.len() == 1, "expected one deduplicated received file");
                    let bytes = std::fs::read(&files[0])?;
                    anyhow::ensure!(bytes.len() == 8 * 1024 * 1024, "wrong received size");
                    anyhow::ensure!(hex::encode(Sha256::digest(&bytes)) == request["sha256"].as_str().context("expected hash")?, "wrong resumed content");
                    anyhow::ensure!(h.actions.phone_notices.lock().unwrap().len() >= 2, "missing mirrored updates");
                    anyhow::ensure!(h.actions.phone_removed.lock().unwrap().len() >= 2, "missing mirrored cleanup");
                    anyhow::ensure!(folder_evidence.len() == 2, "missing real folder scenarios");
                    let evidence = serde_json::json!({"resume_checkpoint":checkpoint,"resumed_offset":request["resumedOffset"],"received_bytes":bytes.len(),"sha256":request["sha256"],"battery":true,"ring":true,"notifications":true,"folders":folder_evidence,"phone_folder_reason":"Can't receive folders yet"});
                    std::fs::write(ready.with_file_name("evidence.json"), serde_json::to_vec_pretty(&evidence)?)?;
                    done = true;
                    evidence
                }
                _ => anyhow::bail!("unknown interop action"),
            };
            let mut bytes = serde_json::to_vec(&value)?;
            bytes.push(b'\n');
            stream.get_mut().write_all(&bytes).await?;
            if done { break; }
        }
        Ok::<(), anyhow::Error>(())
    }).await;
    stop.send_replace(true);
    ipc.await??;
    h.stop().await?;
    result??;
    Ok(())
}
