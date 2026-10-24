//! Login startup belongs to SMAppService.mainApp. A separate, session-only
//! launchd job supervises the daemon; it is never registered as a login item.
//! KeepAlive implies RunAtLoad, even with Crashed, so that job must not persist
//! into the next login when the main app's login registration is off.
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginItemStatus {
    NotRegistered,
    Enabled,
    RequiresApproval,
    NotFound,
    Unknown(isize),
}

impl LoginItemStatus {
    /// macOS also reports NotFound for a valid bundle before its first
    /// registration. Let register distinguish that from an invalid plist.
    pub fn needs_registration(self) -> bool {
        matches!(self, Self::NotRegistered | Self::NotFound)
    }
}

pub fn status_from_raw(status: isize) -> LoginItemStatus {
    match status {
        0 => LoginItemStatus::NotRegistered,
        1 => LoginItemStatus::Enabled,
        2 => LoginItemStatus::RequiresApproval,
        3 => LoginItemStatus::NotFound,
        other => LoginItemStatus::Unknown(other),
    }
}

impl fmt::Display for LoginItemStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotRegistered => f.write_str("not registered"),
            Self::Enabled => f.write_str("enabled"),
            Self::RequiresApproval => f.write_str(
                "requires approval: open System Settings → General → Login Items and allow Pneu",
            ),
            Self::NotFound => f.write_str("not found"),
            Self::Unknown(value) => write!(f, "unknown login item status ({value})"),
        }
    }
}

#[cfg(target_os = "macos")]
mod native {
    use super::*;
    use anyhow::{Context, Result, ensure};
    use objc2::{
        class, msg_send,
        rc::{Retained, autoreleasepool},
        runtime::{AnyObject, Bool},
    };
    use objc2_foundation::{NSBundle, NSError, NSString, NSUserDefaults};

    #[link(name = "ServiceManagement", kind = "framework")]
    unsafe extern "C" {}

    pub fn label() -> Result<String> {
        let bundle = NSBundle::mainBundle();
        let identifier = bundle
            .bundleIdentifier()
            .context("login items require the executable inside Pneu.app")?
            .to_string();
        ensure!(
            matches!(
                identifier.as_str(),
                "io.github.stantonmatt.pneu"
                    | "io.github.stantonmatt.pneu.test"
                    | "io.github.stantonmatt.pneu.test.pkg"
                    | "io.github.stantonmatt.pneu.test.pkg2"
            ),
            "unexpected Pneu bundle identifier: {identifier}"
        );
        Ok(format!("{identifier}.daemon"))
    }

    // The login setting belongs to the bundle/user, not a data-directory
    // override. Finder and the login launcher do not inherit shell PNEU_* vars.
    const INITIALIZED: &str = "PneuLoginItemInitialized";

    pub fn initialized() -> bool {
        NSUserDefaults::standardUserDefaults().boolForKey(&NSString::from_str(INITIALIZED))
    }

    pub fn mark_initialized() {
        NSUserDefaults::standardUserDefaults()
            .setBool_forKey(true, &NSString::from_str(INITIALIZED));
    }

    pub fn main_app(operation: &str) -> Result<LoginItemStatus> {
        ensure!(
            matches!(operation, "register" | "unregister" | "status"),
            "expected login-item register|unregister|status"
        );
        autoreleasepool(|_| {
            label()?; // Restrict ServiceManagement operations to Pneu bundles.
            // SMAppService is available since macOS 13; our bundle requires 15.4.
            // The service is retained across all messages; NSError is borrowed
            // only inside this autorelease pool.
            let service: Retained<AnyObject> =
                unsafe { msg_send![class!(SMAppService), mainAppService] };
            let before: isize = unsafe { msg_send![&*service, status] };
            let before = status_from_raw(before);
            if (operation == "register"
                && matches!(
                    before,
                    LoginItemStatus::Enabled | LoginItemStatus::RequiresApproval
                ))
                || (operation == "unregister" && before.needs_registration())
            {
                return Ok(if operation == "unregister" {
                    LoginItemStatus::NotRegistered
                } else {
                    before
                });
            }
            if operation != "status" {
                let mut error: *mut NSError = std::ptr::null_mut();
                let success: Bool = unsafe {
                    if operation == "register" {
                        msg_send![&*service, registerAndReturnError: &mut error]
                    } else {
                        msg_send![&*service, unregisterAndReturnError: &mut error]
                    }
                };
                ensure!(
                    success.as_bool(),
                    "SMAppService {operation} failed: {}",
                    unsafe { error.as_ref() }
                        .map(|e| format!(
                            "{} ({} {})",
                            e.localizedDescription(),
                            e.domain(),
                            e.code()
                        ))
                        .unwrap_or_else(|| "unknown error".to_owned())
                );
            }
            let raw: isize = unsafe { msg_send![&*service, status] };
            Ok(status_from_raw(raw))
        })
    }
}

#[cfg(target_os = "macos")]
pub use native::label;

#[cfg(target_os = "macos")]
pub fn perform(operation: &str) -> anyhow::Result<LoginItemStatus> {
    let status = native::main_app(operation)?;
    if operation != "status" {
        native::mark_initialized();
    }
    Ok(status)
}

/// Default on exactly once. Subsequent opens respect both our toggle and
/// changes made in System Settings; an absent registration is not permission
/// to register again.
#[cfg(target_os = "macos")]
fn initialize() -> anyhow::Result<LoginItemStatus> {
    if native::initialized() {
        perform("status")
    } else {
        perform("register")
    }
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\"', "&quot;")
        .replace('\'', "&apos;")
}

/// This plist is bootstrapped into the current GUI domain only. It is stored
/// outside ~/Library/LaunchAgents and the bundle's Library/LaunchAgents, so
/// neither launchd nor ServiceManagement discovers it at the next login.
fn session_plist(
    paths: &crate::Paths,
    label: &str,
    executable: &std::path::Path,
) -> anyhow::Result<String> {
    use anyhow::Context;
    let mut settings = String::from("<key>EnvironmentVariables</key><dict>");
    for (key, value) in [
        ("PNEU_DATA_DIR", paths.data.as_path()),
        (
            "PNEU_CONFIG_DIR",
            paths.config.parent().context("config parent")?,
        ),
        ("PNEU_CACHE_DIR", paths.cache.as_path()),
        (
            "PNEU_RUNTIME_DIR",
            paths.socket.parent().context("socket parent")?,
        ),
    ] {
        settings.push_str(&format!(
            "<key>{key}</key><string>{}</string>",
            xml(value.to_str().context("Pneu path is not UTF-8")?)
        ));
    }
    settings.push_str("</dict>");
    let log = paths.socket.with_file_name("pneud.log");
    settings.push_str(&format!(
        "<key>StandardErrorPath</key><string>{}</string>",
        xml(log.to_str().context("log path is not UTF-8")?)
    ));
    Ok(
        include_str!("../../../../packaging/macos/io.github.stantonmatt.pneu.daemon.plist.in")
            .replace("@BUNDLE_ID@.daemon", &xml(label))
            .replace(
                "@EXECUTABLE@",
                &xml(executable
                    .to_str()
                    .context("executable path is not UTF-8")?),
            )
            .replace("<!-- @SESSION_PATHS@ -->", &settings),
    )
}

/// Foundation derives the main bundle from argv[0], which can point outside
/// the app when ~/.local/bin/pneud is a symlink. Re-exec once with an absolute,
/// canonical argv[0] before asking ServiceManagement about the containing app.
#[cfg(target_os = "macos")]
pub fn enter_bundle() -> anyhow::Result<()> {
    use anyhow::Context;
    use std::os::unix::process::CommandExt;
    let executable = std::env::current_exe()
        .context("locate Pneu executable")?
        .canonicalize()
        .context("resolve bundled Pneu executable")?;
    if std::env::args_os().next().as_deref() != Some(executable.as_os_str()) {
        let error = std::process::Command::new(&executable)
            .args(std::env::args_os().skip(1))
            .exec();
        return Err(error).context("re-execute bundled Pneu executable");
    }
    Ok(())
}

#[cfg(target_os = "macos")]
pub fn cli(args: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(
        args.len() == 1,
        "expected login-item register|unregister|status|initialize"
    );
    enter_bundle()?;
    let status = if args[0] == "initialize" {
        initialize()?
    } else {
        perform(&args[0])?
    };
    println!("{status}");
    Ok(())
}

#[cfg(target_os = "macos")]
async fn launchctl(args: &[&str]) -> anyhow::Result<std::process::Output> {
    use anyhow::Context;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new("/bin/launchctl")
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("launchctl timed out")?
    .context("run launchctl")
}

/// LaunchServices has no terminal for startup errors. Present the same error
/// that the CLI reports, including malformed configuration, on the main thread.
#[cfg(target_os = "macos")]
pub fn show_startup_error(error: &anyhow::Error) {
    use objc2::{class, msg_send, rc::Retained, runtime::AnyObject};
    use objc2_foundation::NSString;
    if objc2::MainThreadMarker::new().is_none() {
        return;
    }
    let title = NSString::from_str("Pneu couldn’t start");
    let detail = NSString::from_str(&format!("{error:#}"));
    unsafe {
        let application: Retained<AnyObject> = msg_send![class!(NSApplication), sharedApplication];
        let _: () = msg_send![&*application, activateIgnoringOtherApps: objc2::runtime::Bool::YES];
        let alert: Retained<AnyObject> = msg_send![class!(NSAlert), new];
        let _: () = msg_send![&*alert, setMessageText: &*title];
        let _: () = msg_send![&*alert, setInformativeText: &*detail];
        let _: isize = msg_send![&*alert, runModal];
    }
}

/// Compare launchd's loaded executable and effective directory environment,
/// independently of the newly written plist. Never replace a different live
/// bundle, or interrupt a running job to change its directory configuration.
fn stale_session_job(
    state: &str,
    executable: &std::path::Path,
    paths: &crate::Paths,
) -> anyhow::Result<bool> {
    use anyhow::{Context, ensure};
    let program = state
        .lines()
        .find_map(|line| line.trim().strip_prefix("program = "))
        .context("launchd job has no executable; refusing to replace it")?;
    let program = std::path::Path::new(program);
    ensure!(program.is_absolute(), "launchd executable is not absolute");
    let resolved = program.canonicalize();
    let same_program = resolved.as_deref().unwrap_or(program) == executable;
    let mut environment = std::collections::HashMap::new();
    let mut in_environment = false;
    for line in state.lines().map(str::trim) {
        if matches!(
            line,
            "inherited environment = {" | "default environment = {" | "environment = {"
        ) {
            in_environment = true;
        } else if line == "}" {
            in_environment = false;
        } else if in_environment && let Some((key, value)) = line.split_once(" => ") {
            environment.insert(key, value);
        }
    }
    let same_environment = [
        ("PNEU_DATA_DIR", paths.data.as_path()),
        (
            "PNEU_CONFIG_DIR",
            paths.config.parent().context("config parent")?,
        ),
        ("PNEU_CACHE_DIR", paths.cache.as_path()),
        (
            "PNEU_RUNTIME_DIR",
            paths.socket.parent().context("runtime parent")?,
        ),
    ]
    .into_iter()
    .all(|(key, path)| environment.get(key).copied() == path.to_str());
    if same_program && same_environment {
        return Ok(false);
    }
    ensure!(
        !state.lines().any(|line| {
            let line = line.trim();
            line.starts_with("pid = ") || line == "state = running" || line == "state = spawning"
        }),
        "a Pneu session job is running with a different executable or directory configuration; quit it before relaunching"
    );
    ensure!(
        same_program
            || resolved
                .as_ref()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
        "the Pneu session job belongs to a different live bundle; quit and uninstall that bundle first"
    );
    Ok(true)
}

/// A LaunchServices invocation is a request to show the existing daemon UI.
/// Only launchd starts the daemon; a second direct daemon is never spawned.
#[cfg(target_os = "macos")]
pub async fn launch(paths: &crate::Paths) -> anyhow::Result<()> {
    use anyhow::{Context, bail, ensure};
    use std::time::Duration;
    async fn show(paths: &crate::Paths) -> anyhow::Result<()> {
        crate::ipc::request(
            &paths.socket,
            crate::ipc::Command::ShowWindow {
                choose_folder: false,
            },
        )
        .await?;
        Ok(())
    }
    // Only an absent/refused socket means the daemon needs starting. A daemon
    // rejection or a stalled IPC request must not change login registration.
    match tokio::time::timeout(Duration::from_secs(5), show(paths)).await {
        Ok(Ok(())) => return Ok(()),
        Ok(Err(error))
            if error.chain().any(|cause| {
                cause.downcast_ref::<std::io::Error>().is_some_and(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    )
                })
            }) => {}
        Ok(Err(error)) => return Err(error),
        Err(_) => bail!("running Pneu daemon did not answer ShowWindow within five seconds"),
    }
    paths.prepare_runtime()?;
    crate::config::private_dir(paths.config.parent().context("config parent")?)?;
    crate::Config::load(&paths.config).context("Pneu configuration is invalid")?;
    // A pending login approval must not prevent use in the current session.
    initialize()?;
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    let target = format!("{domain}/{}", label()?);
    let plist = paths.socket.with_file_name("daemon-session.plist");
    let executable = std::env::current_exe()?.canonicalize()?;
    crate::config::atomic_private(
        &plist,
        session_plist(paths, &label()?, &executable)?.as_bytes(),
    )?;
    let mut loaded = launchctl(&["print", &target]).await?;
    if loaded.status.success()
        && stale_session_job(std::str::from_utf8(&loaded.stdout)?, &executable, paths)?
    {
        // Recheck immediately before bootout in case the old job started while
        // we rendered its replacement plist. Never kickstart the stale path.
        loaded = launchctl(&["print", &target]).await?;
        if loaded.status.success()
            && stale_session_job(std::str::from_utf8(&loaded.stdout)?, &executable, paths)?
        {
            let output = launchctl(&["bootout", &target]).await?;
            ensure!(
                output.status.success(),
                "launchctl bootout stale session job: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            loaded = launchctl(&["print", &target]).await?;
            ensure!(
                !loaded.status.success(),
                "stale session job remained loaded after bootout"
            );
        }
    }
    if !loaded.status.success() {
        let output = launchctl(&[
            "bootstrap",
            &domain,
            plist.to_str().context("session plist path")?,
        ])
        .await?;
        let loaded = launchctl(&["print", &target]).await?;
        ensure!(
            loaded.status.success()
                && !stale_session_job(std::str::from_utf8(&loaded.stdout)?, &executable, paths)?,
            "launchctl bootstrap: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    // launchctl can wait for the job to finish starting. A timeout/error here
    // is not proof that launchd rejected the request: bootstrap's RunAtLoad
    // may also be starting it. IPC readiness decides whether launch succeeded.
    let kick_error = match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::process::Command::new("/bin/launchctl")
            .args(["kickstart", &target])
            .kill_on_drop(true)
            .output(),
    )
    .await
    {
        Ok(Ok(output)) if output.status.success() => None,
        Ok(Ok(output)) => Some(format!(
            "launchctl kickstart failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )),
        Ok(Err(error)) => Some(format!("launchctl kickstart failed: {error}")),
        Err(_) => Some("launchctl kickstart timed out".to_owned()),
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if show(paths).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(125)).await;
        }
    })
    .await
    .with_context(|| {
        let message =
            "Pneu daemon did not show its window within ten seconds; check the configuration and run/pneud.log";
        kick_error.map_or_else(|| message.to_owned(), |error| format!("{error}; {message}"))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths() -> crate::Paths {
        crate::Paths {
            data: "/private/pneu/data".into(),
            cache: "/private/pneu/cache".into(),
            config: "/private/pneu/config/config.toml".into(),
            socket: "/private/pneu/run/pneud.sock".into(),
        }
    }
    fn test_environment(paths: &crate::Paths) -> String {
        format!(
            "environment = {{\nPNEU_DATA_DIR => {}\nPNEU_CONFIG_DIR => {}\nPNEU_CACHE_DIR => {}\nPNEU_RUNTIME_DIR => {}\n}}\n",
            paths.data.display(),
            paths.config.parent().unwrap().display(),
            paths.cache.display(),
            paths.socket.parent().unwrap().display()
        )
    }
    #[test]
    fn stopped_job_reloads_for_each_directory_override_and_running_job_is_preserved()
    -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let executable = temp.path().canonicalize()?;
        let paths = test_paths();
        let state = format!(
            "state = not running\nprogram = {}\n{}",
            executable.display(),
            test_environment(&paths)
        );
        assert!(!stale_session_job(&state, &executable, &paths)?);
        for field in ["data", "config", "cache", "run"] {
            let changed = state.replace(
                &format!("/private/pneu/{field}"),
                &format!("/private/other/{field}"),
            );
            assert!(stale_session_job(&changed, &executable, &paths)?, "{field}");
            assert!(
                stale_session_job(&format!("{changed}pid = 123\n"), &executable, &paths).is_err()
            );
        }
        let missing = format!("program = {}\n", executable.display());
        assert!(stale_session_job(&missing, &executable, &paths)?);
        Ok(())
    }
    #[test]
    fn relocation_reloads_only_missing_stopped_executables() -> anyhow::Result<()> {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir()?;
        let current = temp.path().join("new Pneu.app/pneud");
        std::fs::create_dir_all(current.parent().unwrap())?;
        std::fs::write(&current, "new")?;
        let current = current.canonicalize()?;
        let old = temp.path().join("old Pneu.app/pneud");
        let paths = test_paths();
        let environment = test_environment(&paths);
        let stopped = format!(
            "state = not running\nprogram = {}\n{environment}",
            old.display()
        );
        assert!(stale_session_job(&stopped, &current, &paths)?);
        assert!(stale_session_job(&format!("{stopped}pid = 123\n"), &current, &paths).is_err());
        assert!(
            stale_session_job(&format!("{stopped}state = spawning\n"), &current, &paths).is_err()
        );
        std::fs::create_dir_all(old.parent().unwrap())?;
        std::fs::write(&old, "other bundle")?;
        assert!(stale_session_job(&stopped, &current, &paths).is_err());
        std::fs::remove_file(&old)?;
        symlink(&current, &old)?;
        assert!(!stale_session_job(&stopped, &current, &paths)?);
        assert!(!stale_session_job(
            &format!("program = {}\npid = 456\n{environment}", current.display()),
            &current,
            &paths
        )?);
        assert!(stale_session_job("state = not running", &current, &paths).is_err());
        Ok(())
    }

    #[test]
    fn session_supervision_restarts_crashes_without_persisting_login_startup() -> anyhow::Result<()>
    {
        let paths = crate::Paths {
            data: "/private/pneu/data".into(),
            cache: "/private/pneu/cache".into(),
            config: "/private/pneu/config/config.toml".into(),
            socket: "/private/pneu/run/pneud.sock".into(),
        };
        let plist = session_plist(
            &paths,
            "io.github.stantonmatt.pneu.test.pkg.daemon",
            std::path::Path::new("/test & <bundle>/pneud"),
        )?;
        assert!(plist.contains("<key>Crashed</key><true/>"));
        assert!(!plist.contains("SuccessfulExit"));
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
        assert!(
            plist.contains("<key>Program</key><string>/test &amp; &lt;bundle&gt;/pneud</string>")
        );
        assert!(!plist.contains('@'));
        for path in [
            "/private/pneu/data",
            "/private/pneu/cache",
            "/private/pneu/config",
            "/private/pneu/run",
        ] {
            assert!(plist.contains(path));
        }
        Ok(())
    }

    #[test]
    fn smappservice_status_values_and_guidance() {
        assert_eq!(status_from_raw(0), LoginItemStatus::NotRegistered);
        assert_eq!(status_from_raw(1), LoginItemStatus::Enabled);
        assert_eq!(status_from_raw(2), LoginItemStatus::RequiresApproval);
        assert_eq!(status_from_raw(3), LoginItemStatus::NotFound);
        assert_eq!(status_from_raw(99), LoginItemStatus::Unknown(99));
        assert!(status_from_raw(0).needs_registration());
        assert!(status_from_raw(3).needs_registration());
        assert!(!status_from_raw(1).needs_registration());
        assert!(!status_from_raw(2).needs_registration());
        assert!(!status_from_raw(99).needs_registration());
        assert!(
            status_from_raw(2)
                .to_string()
                .contains("System Settings → General → Login Items")
        );
        assert_eq!(status_from_raw(0).to_string(), "not registered");
        assert_eq!(status_from_raw(1).to_string(), "enabled");
        assert!(status_from_raw(3).to_string().starts_with("not found"));
    }
}
