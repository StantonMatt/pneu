//! Local, mutually authenticated pipes. Keep one instance alive at every accept.
use super::*;
use std::{
    mem::size_of,
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle},
    path::PathBuf,
    ptr::null_mut,
};
use tokio::net::windows::named_pipe::{
    ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
};
use windows_sys::Win32::{
    Foundation::{
        ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED, HANDLE,
        LocalFree,
    },
    Security::{Authorization::*, *},
    System::{IO::CancelIoEx, Pipes::*, Threading::*},
};

#[derive(Debug, PartialEq, Eq)]
struct Identity {
    sid: String,
    session: u32,
}
fn win(ok: i32) -> Result<()> {
    ensure!(ok != 0, "Windows IPC: {}", std::io::Error::last_os_error());
    Ok(())
}
struct Local(*mut std::ffi::c_void);
impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: allocations come from the Win32 LocalAlloc family.
        unsafe { LocalFree(self.0) };
    }
}
fn own() -> Result<Identity> {
    // SAFETY: current-process pseudo handle is borrowed; the query-only token
    // is owned immediately and closed on every path;
    // token buffers are aligned and retained while their SID is converted.
    unsafe {
        let process = GetCurrentProcess();
        ensure!(
            !process.is_null(),
            "query own IPC process: {}",
            std::io::Error::last_os_error()
        );
        let mut token = null_mut();
        win(OpenProcessToken(process, TOKEN_QUERY, &mut token))?;
        let token = OwnedHandle::from_raw_handle(token);
        let mut len = 0;
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut len);
        ensure!(
            len as usize >= size_of::<TOKEN_USER>(),
            "query own IPC token size"
        );
        let mut buf = vec![0u64; (len as usize).div_ceil(size_of::<u64>())];
        win(GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            buf.as_mut_ptr().cast(),
            len,
            &mut len,
        ))?;
        let user = &*buf.as_ptr().cast::<TOKEN_USER>();
        let mut text = null_mut();
        win(ConvertSidToStringSidW(user.User.Sid, &mut text))?;
        let _allocation = Local(text.cast());
        let mut len = 0;
        while *text.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16(std::slice::from_raw_parts(text, len))?;
        let mut session = 0u32;
        let mut len = 0;
        win(GetTokenInformation(
            token.as_raw_handle(),
            TokenSessionId,
            (&mut session as *mut u32).cast(),
            size_of::<u32>() as u32,
            &mut len,
        ))?;
        Ok(Identity { sid, session })
    }
}
pub(crate) fn pipe_name(runtime_override: Option<&Path>) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::os::windows::ffi::OsStrExt;
    let sid = own()?.sid;
    // Override directories are namespaces, not pipe filesystem parents. Use a
    // stable digest so independent test daemons never share the ordinary pipe.
    let suffix = runtime_override
        .map(|path| {
            let bytes: Vec<_> = path
                .as_os_str()
                .encode_wide()
                .flat_map(u16::to_le_bytes)
                .collect();
            format!("-{}", hex::encode(Sha256::digest(bytes)))
        })
        .unwrap_or_default();
    Ok(PathBuf::from(format!(r"\\.\pipe\pneu-{sid}{suffix}")))
}
// Expectations are per connection/listener so tests never mutate process-wide
// identity or affect another concurrently running test.
struct PeerCheck {
    expected: Identity,
    #[cfg(test)]
    fail_query: bool,
    #[cfg(test)]
    fail_security_query: bool,
}
impl PeerCheck {
    fn own() -> Result<Self> {
        Ok(Self {
            expected: own()?,
            #[cfg(test)]
            fail_query: false,
            #[cfg(test)]
            fail_security_query: false,
        })
    }
    fn session(&self, handle: HANDLE, server: bool) -> Result<()> {
        #[cfg(test)]
        let handle = if self.fail_query { null_mut() } else { handle };
        let mut session = 0;
        // SAFETY: production callers retain the connected pipe. Tests also use
        // NULL to exercise the actual Win32 query-failure path.
        unsafe {
            win(if server {
                GetNamedPipeServerSessionId(handle, &mut session)
            } else {
                GetNamedPipeClientSessionId(handle, &mut session)
            })
            .context("query IPC pipe session")?;
        }
        ensure!(session == self.expected.session, "foreign IPC session");
        Ok(())
    }
    fn server(&self, handle: HANDLE) -> Result<()> {
        #[cfg(test)]
        let handle = if self.fail_security_query {
            null_mut()
        } else {
            handle
        };
        check_pipe_security(handle, &self.expected.sid)?;
        self.session(handle, true)
    }
}
fn check_pipe_security(handle: HANDLE, sid: &str) -> Result<()> {
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
    let mut owner = null_mut();
    let mut dacl = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: the live handle and valid outputs are retained through comparison.
    // GetSecurityInfo allocates the descriptor; returned SID/ACLs borrow it.
    unsafe {
        let status = GetSecurityInfo(
            handle,
            SE_KERNEL_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        );
        ensure!(
            status == 0,
            "query IPC pipe security: {}",
            std::io::Error::from_raw_os_error(status as i32)
        );
        let _descriptor = Local(descriptor);
        let mut control = 0;
        let mut revision = 0;
        win(GetSecurityDescriptorControl(
            descriptor,
            &mut control,
            &mut revision,
        ))?;
        ensure!(
            control & SE_DACL_PROTECTED != 0 && !dacl.is_null(),
            "unprotected IPC pipe DACL"
        );
        ensure!((*dacl).AceCount == 2, "unexpected IPC pipe grants");
        let mut matched = [false; 2];
        for (index, text) in [sid, "S-1-5-18"].into_iter().enumerate() {
            let text: Vec<_> = text.encode_utf16().chain(Some(0)).collect();
            let mut expected = null_mut();
            win(ConvertStringSidToSidW(text.as_ptr(), &mut expected))?;
            let _expected = Local(expected);
            if index == 0 {
                ensure!(
                    !owner.is_null() && EqualSid(owner, expected) != 0,
                    "foreign IPC pipe owner"
                );
            }
            for n in 0..2 {
                let mut ace = null_mut();
                win(GetAce(dacl, n, &mut ace))?;
                let header = &*ace.cast::<ACE_HEADER>();
                ensure!(
                    header.AceType == 0
                        && header.AceFlags == 0
                        && header.AceSize as usize >= size_of::<ACCESS_ALLOWED_ACE>(),
                    "unexpected IPC pipe ACE"
                );
                let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                ensure!(
                    ace.Mask == FILE_ALL_ACCESS,
                    "unexpected IPC pipe access mask"
                );
                if EqualSid((&ace.SidStart as *const u32).cast_mut().cast(), expected) != 0 {
                    matched[index] = true;
                }
            }
        }
        ensure!(
            matched == [true, true],
            "IPC pipe must grant only owner and SYSTEM"
        );
    }
    Ok(())
}
pub(super) async fn connect(path: &Path) -> Result<NamedPipeClient> {
    connect_checked(path, &PeerCheck::own()?).await
}
async fn connect_checked(path: &Path, check: &PeerCheck) -> Result<NamedPipeClient> {
    loop {
        // Keep Tokio's default identification-level security QOS: a server must
        // never be able to impersonate the connecting client's token.
        match ClientOptions::new().open(path) {
            Ok(client) => {
                // The object owner/DACL and endpoint session are connection-bound.
                // A PID/token lookup adds no authority and is vulnerable to PID reuse.
                check.server(client.as_raw_handle())?;
                return Ok(client);
            }
            Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                // The shared request timeout bounds this wait.
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(error.into()),
        }
    }
}
/// Disconnect before drop, including rejection, handler overflow, cancellation,
/// and listener failure. Mio otherwise retains pending writes and their buffers.
pub(super) struct Connection(pub(super) NamedPipeServer);
impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.0.disconnect();
    }
}
// Clients have no DisconnectNamedPipe operation. Cancel their overlapped I/O
// before drop so a cancelled/failed request cannot retain a queued large write.
pub(super) struct ClientConnection(pub(super) NamedPipeClient);
impl Drop for ClientConnection {
    fn drop(&mut self) {
        // SAFETY: live owned handle; NULL cancels this handle's pending I/O.
        unsafe { CancelIoEx(self.0.as_raw_handle(), std::ptr::null()) };
    }
}
fn instance(path: &Path, first: bool, sid: &str) -> Result<NamedPipeServer> {
    let sddl: Vec<_> = format!("O:{sid}D:P(A;;FA;;;{sid})(A;;FA;;;SY)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = null_mut();
    // SAFETY: NUL-terminated SDDL and valid outputs. The descriptor remains live
    // through synchronous pipe creation, which copies the security attributes.
    unsafe {
        win(ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        ))?;
        let _descriptor = Local(descriptor);
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        Ok(ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                path,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )?)
    }
}
pub struct Listener {
    pending: Connection,
    path: PathBuf,
    sid: String,
    check: PeerCheck,
    #[cfg(test)]
    next_check: Option<PeerCheck>,
}
impl Listener {
    fn claim(path: &Path) -> Result<Self> {
        let sid = own()?.sid;
        let pending = instance(path, true, &sid).context(
            "cannot claim IPC pipe name; another process may own it (pipe-name squatting or another daemon)",
        )?;
        Ok(Self {
            pending: Connection(pending),
            path: path.to_owned(),
            sid,
            check: PeerCheck::own()?,
            #[cfg(test)]
            next_check: None,
        })
    }
}
pub(super) async fn accept(listener: &mut Listener) -> Result<Connection> {
    loop {
        let connected = match listener.pending.0.connect().await {
            Ok(()) => true,
            // A client can disconnect before accept is polled. Replace that
            // instance without ending the daemon or surrendering its name.
            Err(error)
                if matches!(
                    error.raw_os_error().map(|code| code as u32),
                    Some(ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED | ERROR_BROKEN_PIPE)
                ) =>
            {
                false
            }
            Err(error) => return Err(error.into()),
        };
        // Create the replacement BEFORE transferring or discarding the old
        // instance, including rejected peers and the handler limit.
        let next =
            instance(&listener.path, false, &listener.sid).context("retain IPC pipe ownership")?;
        let previous = std::mem::replace(&mut listener.pending, Connection(next));
        // Mio may buffer up to 4 KiB before this session check. Rejected bytes
        // are discarded on disconnect and are never parsed by the application.
        // The protected creation DACL already restricts openers to our SID/SYSTEM.
        let check = &listener.check;
        #[cfg(test)]
        let next_check = listener.next_check.take();
        #[cfg(test)]
        let check = next_check.as_ref().unwrap_or(check);
        if connected && check.session(previous.0.as_raw_handle(), false).is_ok() {
            return Ok(previous);
        }
    }
}
pub struct SocketOwner {
    _lock: std::fs::File,
}
pub fn bind(path: &Path, data: &Path) -> Result<(Listener, SocketOwner)> {
    crate::config::private_dir(data)?;
    let lock = crate::platform::fs::open_lock(&data.join("daemon.lock"))?;
    lock.try_lock()
        .map_err(crate::platform::fs::lock_error)
        .context("pneud is already running; another daemon owns the data-directory lock")?;
    Ok((Listener::claim(path)?, SocketOwner { _lock: lock }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn unique_pipe() -> Result<PathBuf> {
        pipe_name(Some(Path::new(&format!(
            r"C:\pneu-ipc-test-{}",
            crate::random_hex(12)
        ))))
    }
    #[test]
    fn pipe_namespace_is_per_user_and_override() -> Result<()> {
        let default = pipe_name(None)?;
        assert!(default.to_string_lossy().contains(&own()?.sid));
        let a = Path::new(r"C:\pneu-test-a");
        assert_eq!(pipe_name(Some(a))?, pipe_name(Some(a))?);
        assert_ne!(default, pipe_name(Some(a))?);
        assert_ne!(
            pipe_name(Some(a))?,
            pipe_name(Some(Path::new(r"C:\pneu-test-b")))?
        );
        Ok(())
    }
    #[tokio::test]
    async fn pipe_descriptor_is_protected_and_owner_system_only() -> Result<()> {
        use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;
        let listener = Listener::claim(&unique_pipe()?)?;
        let mut owner = null_mut();
        let mut dacl = null_mut();
        let mut descriptor = null_mut();
        // SAFETY: live pipe, valid output pointers. All returned SID/ACL
        // pointers are retained by the descriptor until the final comparison.
        unsafe {
            assert_eq!(
                GetSecurityInfo(
                    listener.pending.0.as_raw_handle(),
                    SE_KERNEL_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    &mut owner,
                    null_mut(),
                    &mut dacl,
                    null_mut(),
                    &mut descriptor
                ),
                0
            );
            let _descriptor = Local(descriptor);
            let mut control = 0;
            let mut revision = 0;
            win(GetSecurityDescriptorControl(
                descriptor,
                &mut control,
                &mut revision,
            ))?;
            assert_ne!(control & SE_DACL_PROTECTED, 0);
            assert!(!dacl.is_null());
            assert_eq!((*dacl).AceCount, 2);
            for (index, sid) in [own()?.sid, "S-1-5-18".into()].into_iter().enumerate() {
                let text: Vec<_> = sid.encode_utf16().chain(Some(0)).collect();
                let mut expected = null_mut();
                win(ConvertStringSidToSidW(text.as_ptr(), &mut expected))?;
                let _expected = Local(expected);
                if index == 0 {
                    assert_ne!(EqualSid(owner, expected), 0);
                }
                let mut ace = null_mut();
                win(GetAce(dacl, index as u32, &mut ace))?;
                let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                assert_eq!(ace.Header.AceType, 0); // ACCESS_ALLOWED_ACE_TYPE
                assert_eq!(ace.Header.AceFlags, 0);
                assert_eq!(ace.Mask, FILE_ALL_ACCESS);
                assert_ne!(
                    EqualSid((&ace.SidStart as *const u32).cast_mut().cast(), expected),
                    0
                );
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn pipe_claim_detects_squatting_and_retains_continuous_ownership() -> Result<()> {
        let name = unique_pipe()?;
        let mut listener = Listener::claim(&name)?;
        assert!(
            Listener::claim(&name)
                .err()
                .unwrap()
                .to_string()
                .contains("another process")
        );
        for _ in 0..8 {
            let client = connect(&name).await?;
            let server = accept(&mut listener).await?;

            drop(client);
            drop(server);
            assert!(Listener::claim(&name).is_err());
        }
        drop(listener);
        // Mio retains handles until cancelled overlapped reads complete. Let
        // the reactor process those completions before reclaiming the name.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(listener) = Listener::claim(&name) {
                    break drop(listener);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    }
    #[tokio::test]
    async fn data_lock_refuses_second_daemon_even_with_a_different_pipe() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let name = unique_pipe()?;
        let (listener, owner) = bind(&name, dir.path())?;
        let error = bind(&unique_pipe()?, dir.path()).err().unwrap();
        assert!(error.to_string().contains("already running"), "{error:#}");
        drop(listener);
        drop(owner);
        drop(bind(&name, dir.path())?);
        Ok(())
    }
    async fn fixture(
        large: bool,
    ) -> Result<(tempfile::TempDir, PathBuf, Arc<Core>, Listener, SocketOwner)> {
        let dir = crate::test_support::scratch()?;
        let name = unique_pipe()?;
        let paths = crate::Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: name.clone(),
        };
        paths.prepare()?;
        if large {
            let store = crate::store::Store::open(&paths.data.join("state.sqlite3"))?;
            store.test_locked(|c| {
                let tx = c.transaction()?;
                for n in 0..80 {
                    tx.execute(
                        "INSERT INTO peers(id,fp,name) VALUES(?,?,?)",
                        rusqlite::params![
                            format!("{n:032x}"),
                            format!("{n:064x}"),
                            "x".repeat(32_768)
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })?;
        }
        let (listener, owner) = bind(&name, &paths.data)?;
        let core = Core::new(
            paths,
            Config {
                save_path: dir.path().join("saved"),
                ..Config::default()
            },
            Arc::new(crate::actions::DesktopActions::new(
                dir.path().join("cache"),
            )),
        )
        .await?;
        Ok((dir, name, core, listener, owner))
    }
    async fn assert_no_request_bytes(server: &mut NamedPipeServer) -> Result<()> {
        let mut byte = [0; 1];
        match tokio::time::timeout(Duration::from_secs(2), server.read(&mut byte)).await? {
            Ok(n) => assert_eq!(n, 0, "request bytes escaped failed authorization"),
            Err(error) => assert!(
                matches!(
                    error.raw_os_error().map(|n| n as u32),
                    Some(ERROR_BROKEN_PIPE | ERROR_NO_DATA | ERROR_PIPE_NOT_CONNECTED)
                ),
                "{error}"
            ),
        }
        Ok(())
    }
    #[tokio::test]
    async fn request_rejects_owner_session_and_query_failure_before_sending() -> Result<()> {
        for case in 0..4 {
            let name = unique_pipe()?;
            let mut server = Connection(instance(&name, true, &own()?.sid)?);
            let mut check = PeerCheck::own()?;
            match case {
                // Test seam: same-user processes cannot assign a foreign owner
                // without extra privileges; substitute the expected owner SID.
                0 => check.expected.sid = "S-1-5-18".into(),
                1 => check.expected.session = check.expected.session.wrapping_add(1),
                2 => check.fail_security_query = true,
                _ => check.fail_query = true,
            }
            let result = request_connected(
                Command::Rename {
                    name: "must not send".into(),
                },
                connect_checked(&name, &check),
            )
            .await;
            let error = result.unwrap_err();
            assert!(
                format!("{error:#}").contains(match case {
                    0 => "foreign IPC pipe owner",
                    1 => "foreign IPC session",
                    2 => "query IPC pipe security",
                    _ => "query IPC pipe session",
                }),
                "{error:#}"
            );
            server.0.connect().await?;
            assert_no_request_bytes(&mut server.0).await?;
        }
        Ok(())
    }
    #[tokio::test]
    async fn request_rejects_unprotected_pipe_before_sending() -> Result<()> {
        let name = unique_pipe()?;
        let mut server = Connection(
            ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)?,
        );
        let error = request(&name, Command::Status).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("unprotected IPC pipe DACL"),
            "{error:#}"
        );
        server.0.connect().await?;
        assert_no_request_bytes(&mut server.0).await
    }
    #[tokio::test]
    async fn request_rejects_extra_pipe_grants_before_sending() -> Result<()> {
        let name = unique_pipe()?;
        let sid = own()?.sid;
        let sddl: Vec<_> = format!("O:{sid}D:P(A;;FA;;;{sid})(A;;FA;;;SY)(A;;FA;;;WD)")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = null_mut();
        // SAFETY: valid NUL-terminated descriptor and outputs, retained until
        // CreateNamedPipe has copied the intentionally unsafe test ACL.
        let mut server = unsafe {
            win(ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            ))?;
            let _descriptor = Local(descriptor);
            let mut attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            Connection(
                ServerOptions::new()
                    .first_pipe_instance(true)
                    .create_with_security_attributes_raw(
                        &name,
                        (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                    )?,
            )
        };
        let error = request(&name, Command::Status).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("unexpected IPC pipe grants"),
            "{error:#}"
        );
        server.0.connect().await?;
        assert_no_request_bytes(&mut server.0).await
    }
    #[tokio::test]
    async fn serve_rejects_session_and_query_failure_then_accepts_authorized_request() -> Result<()>
    {
        for query_failure in [false, true] {
            let (_dir, name, core, mut listener, _owner) = fixture(false).await?;
            let mut check = PeerCheck::own()?;
            if query_failure {
                check.fail_query = true;
            } else {
                check.expected.session = check.expected.session.wrapping_add(1);
            }
            listener.next_check = Some(check);
            let original = core.config().name;
            let (stop, rx) = tokio::sync::watch::channel(false);
            let server = tokio::spawn(serve(core.clone(), listener, rx));
            let rejected = request(
                &name,
                Command::Rename {
                    name: "must not execute".into(),
                },
            )
            .await;
            let next = request(&name, Command::Status).await;
            stop.send(true)?;
            tokio::time::timeout(Duration::from_secs(2), server).await???;
            core.shutdown_actions().await?;
            assert!(rejected.is_err());
            assert_eq!(core.config().name, original);
            assert!(next?["devices"].is_array());
        }
        Ok(())
    }
    fn instances(client: &NamedPipeClient) -> Result<u32> {
        let mut count = 0;
        // SAFETY: live connected handle and valid optional output.
        unsafe {
            win(GetNamedPipeHandleStateW(
                client.as_raw_handle(),
                null_mut(),
                &mut count,
                null_mut(),
                null_mut(),
                null_mut(),
                0,
            ))?;
        }
        Ok(count)
    }
    async fn wait_for_reclaim(name: &Path) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(listener) = Listener::claim(name) {
                    break drop(listener);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        Ok(())
    }
    async fn nonreading_batch(
        name: &Path,
        stalled: &mut Vec<NamedPipeClient>,
    ) -> Result<NamedPipeClient> {
        // Preparing sixteen large replies can take longer than the write
        // deadline on a busy runner. Freeze that budget until every handler
        // has started writing, without consuming any of its response.
        tokio::time::pause();
        let started = std::time::Instant::now();
        let prepare = async {
            let check = PeerCheck::own()?;
            let open = || async {
                loop {
                    match ClientOptions::new().open(name) {
                        Ok(client) => {
                            check.server(client.as_raw_handle())?;
                            break Ok::<_, anyhow::Error>(client);
                        }
                        Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32) => {
                            // The production connector sleeps between retries;
                            // paused time needs an untimed retry instead.
                            tokio::task::yield_now().await;
                        }
                        Err(error) => break Err(error.into()),
                    }
                }
            };
            let first = stalled.len();
            for _ in 0..16 {
                let mut client = open().await?;
                client.write_all(b"{\"command\":\"status\"}\n").await?;
                stalled.push(client);
            }
            for client in &stalled[first..] {
                loop {
                    let mut available = 0;
                    // SAFETY: live overlapped pipe handle and valid output.
                    // Peek without draining: readiness alone can also mean EOF.
                    unsafe {
                        win(PeekNamedPipe(
                            client.as_raw_handle(),
                            null_mut(),
                            0,
                            null_mut(),
                            &mut available,
                            null_mut(),
                        ))?;
                    }
                    if available > 0 {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            }
            open().await
        };
        let mut prepare = std::pin::pin!(prepare);
        let result = std::future::poll_fn(|cx| {
            if started.elapsed() > Duration::from_secs(60) {
                return std::task::Poll::Ready(Err(anyhow::anyhow!(
                    "non-reading handlers did not start writing within 60 seconds"
                )));
            }
            // Prevent automatic clock advancement while Windows I/O completes.
            // The wall-clock bound above still catches a stalled setup.
            cx.waker().wake_by_ref();
            std::future::Future::poll(prepare.as_mut(), cx)
        })
        .await;
        tokio::time::resume();
        result
    }
    #[tokio::test]
    async fn nonreading_clients_reclaim_instances_and_shutdown_cancels_io() -> Result<()> {
        let (_dir, name, core, listener, _owner) = fixture(true).await?;
        let (stop, rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve(core.clone(), listener, rx));
        let mut stalled = Vec::new();
        // Retain all client endpoints after each deadline: dropping them would
        // hide Mio's pending-write leak. More than 16 total connections must
        // leave only the listener and our measurement probe alive server-side.
        for _ in 0..2 {
            let client = nonreading_batch(&name, &mut stalled).await?;
            let overflow = tokio::time::timeout(
                Duration::from_secs(2),
                request_connected(Command::Status, async { Ok(client) }),
            )
            .await?;
            assert!(
                overflow.is_err(),
                "handler limit must reject the seventeenth active client"
            );
            tokio::time::sleep(Duration::from_secs(11)).await;
            let probe = connect(&name).await?;
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    if instances(&probe)? <= 2 {
                        break Ok::<_, anyhow::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await??;
            drop(probe);
            let reply = request(&name, Command::Status).await?;
            assert_eq!(reply["devices"].as_array().unwrap().len(), 80);
            assert!(serde_json::to_vec(&reply)?.len() > RESPONSE_LIMIT);
        }
        // Shutdown while both an idle read and a large write are outstanding.
        let idle = connect(&name).await?;
        let mut writer = connect(&name).await?;
        writer.write_all(b"{\"command\":\"status\"}\n").await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        stop.send(true)?;
        tokio::time::timeout(Duration::from_secs(2), server).await???;
        wait_for_reclaim(&name).await?;
        // Keep every client alive until after proving server handles reclaimed.
        drop((idle, writer, stalled));
        core.shutdown_actions().await?;
        Ok(())
    }
    #[tokio::test]
    async fn request_roundtrip_and_paged_reply() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let name = unique_pipe()?;
        let paths = crate::Paths {
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            config: dir.path().join("config/config.toml"),
            socket: name.clone(),
        };
        let (listener, _owner) = bind(&name, &paths.data)?;
        let core = Core::new(
            paths,
            Config {
                save_path: dir.path().join("saved"),
                ..Config::default()
            },
            Arc::new(crate::actions::DesktopActions::new(
                dir.path().join("cache"),
            )),
        )
        .await?;
        // A queued, disconnected peer must not terminate the accept loop.
        drop(ClientOptions::new().open(&name)?);
        let (stop, rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(serve(core.clone(), listener, rx));
        let result = request(&name, Command::Status).await;
        let desktop = request(
            &name,
            Command::ShowWindow {
                choose_folder: false,
            },
        )
        .await;
        let _ = stop.send(true);
        server.await??;
        core.shutdown_actions().await?;
        assert!(result?["devices"].is_array());
        if crate::ui::runtime::require_ui(&core).is_ok() {
            desktop?;
            assert_ne!(core.window_request_with_activation().0, 0);
        } else {
            assert!(
                desktop
                    .unwrap_err()
                    .to_string()
                    .contains("interactive Windows session")
            );
        }

        // Drive the production request reader with multiple response pages and
        // verify missing final pages fail instead of returning partial state.
        for complete in [false, true] {
            let name = unique_pipe()?;
            let mut listener = Listener::claim(&name)?;
            let value = serde_json::json!({"items":vec!["x".repeat(32_768); 80], "revision":17});
            let expected = value.clone();
            let server = tokio::spawn(async move {
                let stream = accept(&mut listener).await?;

                let mut stream = stream;
                let mut reader = BufReader::new(&mut stream.0);
                let _: Command = serde_json::from_slice(&line(&mut reader, REQUEST_LIMIT).await?)?;
                let mut pages = response_pages(Reply {
                    ok: true,
                    value,
                    more: false,
                })?;
                assert!(pages.len() > 1);
                if !complete {
                    pages.pop();
                }
                for page in pages {
                    reader
                        .get_mut()
                        .write_all(&serde_json::to_vec(&page)?)
                        .await?;
                    reader.get_mut().write_all(b"\n").await?;
                }
                // Pipe handles must stay alive until the peer consumes queued
                // output. Its close is also bounded so a failed client exits.
                let mut byte = [0; 1];
                let _ =
                    tokio::time::timeout(Duration::from_secs(1), reader.get_mut().read(&mut byte))
                        .await;
                Ok::<_, anyhow::Error>(())
            });
            let result = request(&name, Command::Status).await;
            server.await??;
            if complete {
                assert_eq!(result?, expected);
            } else {
                assert!(result.is_err());
            }
        }
        Ok(())
    }
    #[tokio::test]
    async fn foreign_account_client_is_denied() -> Result<()> {
        // Manual VM driver runs only this test as pneu2, targeting the owner's
        // exact pipe name (the ordinary CLI naturally uses pneu2's own SID).
        if let Some(pipe) = std::env::var_os("PNEU_IPC_FOREIGN_PIPE") {
            let error = request(Path::new(&pipe), Command::Status)
                .await
                .unwrap_err();
            assert_eq!(
                error
                    .downcast_ref::<std::io::Error>()
                    .and_then(std::io::Error::raw_os_error),
                Some(5),
                "expected access denied opening the owner's pipe: {error:#}"
            );
        }
        Ok(())
    }
}
