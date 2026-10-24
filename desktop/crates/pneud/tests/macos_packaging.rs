//! Actual scripts with private files and mocked host tools; no user services.
use anyhow::{Context, Result, ensure};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

struct Sandbox {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    trace: PathBuf,
}
impl Sandbox {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join("checkout with spaces");
        let home = temp.path().join("home");
        let bin = temp.path().join("tools");
        let trace = temp.path().join("trace");
        for p in [&root, &home, &bin, &root.join("packaging/macos")] {
            fs::create_dir_all(p)?;
        }
        let desktop = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        for name in ["install-macos.sh", "uninstall-macos.sh", "Cargo.toml"] {
            fs::copy(desktop.join(name), root.join(name))?;
        }
        for entry in fs::read_dir(desktop.join("packaging/macos"))? {
            let e = entry?;
            fs::copy(e.path(), root.join("packaging/macos").join(e.file_name()))?;
        }
        fs::create_dir_all(root.join("assets/macos/AppIcon.iconset"))?;
        fs::write(
            root.join("assets/macos/AppIcon.iconset/icon_512x512@2x.png"),
            b"mock PNG",
        )?;
        fs::write(root.join("assets/macos/menubar-connected.png"), b"template")?;
        fs::write(
            root.join("assets/macos/menubar-connected@2x.png"),
            b"template retina",
        )?;
        let mock = bin.join("mock");
        fs::write(
            &mock,
            r#"#!/usr/bin/env bash
set -euo pipefail
name=${0##*/}
printf '%s %s\n' "$name" "$*" >> "$PNEU_TRACE"
case "$name" in
    uname) echo "${PNEU_MOCK_OS:-Darwin}" ;;
    heavy) "$@" ;;
    cargo)
        profile=debug
        for arg in "$@"; do if [[ "$arg" == --release ]]; then profile=release; fi; done
        mkdir -p "target/$profile"
        cat > "target/$profile/pneud" <<'BIN'
#!/usr/bin/env bash
printf 'pneud %s\n' "$*" >> "$PNEU_TRACE"
case "$*" in
    'login-item register'|'login-item initialize') echo "${PNEU_MOCK_STATUS:-enabled}" ;;
    'login-item unregister')
        [[ ${PNEU_MOCK_UNREGISTER_FAIL:-false} != true ]] || exit 1
        echo 'not registered' ;;
    'login-item status') echo "${PNEU_MOCK_OLD_STATUS:-enabled}" ;;
    'uninstall-owned '*) exec "$PNEU_TEST_CHECK_BINARY" "$@" ;;
    '') exit 0 ;;
    *) exit 2 ;;
esac
BIN
        chmod +x "target/$profile/pneud"
        ;;
    iconutil) while [[ "$1" != -o ]]; do shift; done; touch "$2" ;;
    qlmanage) while [[ "$1" != -o ]]; do shift; done; touch "$2/pneu.svg.png" ;;
    sips) touch "${@: -1}" ;;
    plutil)
        if [[ "$1" == -extract ]]; then
            if [[ "$(cat "${@: -1}")" == *'<string>io.github.stantonmatt.pneu.test.pkg</string>'* ]]; then echo io.github.stantonmatt.pneu.test.pkg
            elif [[ "$(cat "${@: -1}")" == *'<string>io.github.stantonmatt.pneu.test</string>'* ]]; then echo io.github.stantonmatt.pneu.test
            elif [[ "$(cat "${@: -1}")" == *'<string>io.github.stantonmatt.pneu</string>'* ]]; then echo io.github.stantonmatt.pneu; else echo wrong.bundle; fi
        fi ;;
    codesign) [[ "${PNEU_MOCK_SIGN_FAIL:-false}" != true ]] ;;
    pgrep)
        [[ ${PNEU_MOCK_PROBE_FAIL:-false} != true ]] || exit 2
        [[ -n ${PNEU_MOCK_FOREGROUND_PID:-} ]] || exit 1
        echo "$PNEU_MOCK_FOREGROUND_PID" ;;
    launchctl)
        if [[ "$1" == bootout && -n ${PNEU_MOCK_SERVICE_PID:-} ]]; then
            [[ -f "$PNEU_MOCK_OLD_MARKER" ]]
            kill -TERM "$PNEU_MOCK_SERVICE_PID"
        fi
        if [[ "$1" == print ]]; then
            [[ -n ${PNEU_MOCK_SERVICE_PID:-} ]] || exit 1
            printf 'pid = %s\n' "$PNEU_MOCK_SERVICE_PID"
            printf 'program = %s\n' "${PNEU_MOCK_SERVICE_PROGRAM:-${PNEU_MOCK_OLD_MARKER%/*}/Contents/MacOS/pneud}"
        fi ;;
    *) echo "Unexpected mocked command $name" >&2; exit 1 ;;
esac
"#,
        )?;
        use std::os::unix::fs::{PermissionsExt, symlink};
        fs::set_permissions(&mock, fs::Permissions::from_mode(0o755))?;
        for name in [
            "uname",
            "heavy",
            "cargo",
            "iconutil",
            "plutil",
            "codesign",
            "launchctl",
            "pgrep",
            "qlmanage",
            "sips",
        ] {
            symlink(&mock, bin.join(name))?;
        }
        Ok(Self {
            _temp: temp,
            root,
            home,
            bin,
            trace,
        })
    }
    fn command(&self, script: &str) -> Command {
        let mut c = Command::new("bash");
        c.arg(self.root.join(script))
            .env("HOME", &self.home)
            .env("PNEU_TEST_CHECK_BINARY", env!("CARGO_BIN_EXE_pneud"))
            .env_remove("PNEU_CONFIG_DIR")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("CARGO_TARGET_DIR")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    self.bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("PNEU_TRACE", &self.trace);
        c
    }
    fn run(&self, script: &str, args: &[&str]) -> Result<()> {
        let output = self.command(script).args(args).output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
    fn calls(&self) -> Result<String> {
        Ok(fs::read_to_string(&self.trace)?)
    }
}

#[test]
fn bundle_contract_layout_and_signing() -> Result<()> {
    let s = Sandbox::new()?;
    for profile in ["debug", "release"] {
        s.run(
            "packaging/macos/build-app.sh",
            &[
                "--profile",
                profile,
                "--out",
                s.root.join("bundle out").to_str().context("path")?,
                "--bundle-id",
                "io.github.stantonmatt.pneu.test",
            ],
        )?;
        let app = s.root.join("bundle out/Pneu.app/Contents");
        let info = fs::read_to_string(app.join("Info.plist"))?;
        for expected in [
            "<key>CFBundleIdentifier</key><string>io.github.stantonmatt.pneu.test</string>",
            "<key>CFBundleExecutable</key><string>pneud</string>",
            "<key>CFBundleName</key><string>Pneu</string>",
            "<key>CFBundleDisplayName</key><string>Pneu</string>",
            "<key>CFBundleIconFile</key><string>AppIcon</string>",
            "<key>LSUIElement</key><true/>",
            "<key>LSMinimumSystemVersion</key><string>15.4</string>",
            "<key>NSHighResolutionCapable</key><true/>",
            "Pneu connects to your phone and computers on your network.",
            "<key>NSBonjourServices</key><array><string>_pneu._tcp</string></array>",
            "Pneu uses the camera to scan another computer's pairing code.",
        ] {
            assert!(info.contains(expected), "missing {expected}");
        }
        assert!(!info.contains('@'));
        assert!(info.contains(&format!(
            "<key>CFBundleVersion</key><string>{}</string>",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(info.contains(&format!(
            "<key>CFBundleShortVersionString</key><string>{}</string>",
            env!("CARGO_PKG_VERSION")
        )));
        let agent = fs::read_to_string(app.join("Resources/daemon.plist.in"))?;
        for expected in [
            "<key>Label</key><string>io.github.stantonmatt.pneu.test.daemon</string>",
            "<key>Program</key><string>@EXECUTABLE@</string>",
            "<key>ProgramArguments</key><array><string>pneud</string><string>run</string></array>",
            "<key>RunAtLoad</key><true/>",
            "<key>KeepAlive</key><dict><key>Crashed</key><true/></dict>",
            "<key>LimitLoadToSessionType</key><string>Aqua</string>",
            "<key>ProcessType</key><string>Interactive</string>",
        ] {
            assert!(agent.contains(expected), "missing {expected}");
        }
        assert!(app.join("MacOS/pneud").is_file());
        assert!(app.join("Resources/AppIcon.icns").is_file());
        assert!(
            app.join("Resources/menubar/pneu-tray-connected.png")
                .is_file()
        );
        assert!(
            app.join("Resources/menubar/pneu-tray-connected@2x.png")
                .is_file()
        );
        assert_eq!(fs::read_dir(app.join("MacOS"))?.count(), 1);
    }
    let calls = s.calls()?;
    assert!(calls.contains("heavy cargo build --locked -p pneud --release"));
    assert!(calls.contains("codesign --force --sign - --options runtime"));
    assert!(calls.contains("--entitlements packaging/macos/Pneu.entitlements"));
    let entitlements = fs::read_to_string(s.root.join("packaging/macos/Pneu.entitlements"))?;
    assert!(entitlements.contains("<key>com.apple.security.device.camera</key><true/>"));
    assert!(calls.contains("codesign --verify --strict"));
    assert!(!calls.contains("launchctl"));
    assert!(!calls.contains("login-item"));
    Ok(())
}

#[test]
fn reinstall_and_uninstall_lifecycle() -> Result<()> {
    let s = Sandbox::new()?;
    let data = s.home.join("Library/Application Support/Pneu/keep");
    fs::create_dir_all(data.parent().context("parent")?)?;
    fs::write(&data, "private")?;
    s.run("install-macos.sh", &[])?;
    let app = s.home.join("Applications/Pneu.app");
    fs::write(app.join("Contents/Resources/AppIcon.icns"), "old")?;
    assert!(
        !s.command("install-macos.sh")
            .env("PNEU_MOCK_UNREGISTER_FAIL", "true")
            .output()?
            .status
            .success()
    );
    assert_eq!(
        fs::read_to_string(app.join("Contents/Resources/AppIcon.icns"))?,
        "old"
    );
    s.run("install-macos.sh", &[])?;
    assert_ne!(
        fs::read_to_string(app.join("Contents/Resources/AppIcon.icns"))?,
        "old"
    );
    let calls = s.calls()?;
    let unregister = calls
        .find("pneud login-item unregister")
        .context("unregister")?;
    let register = calls
        .rfind("pneud login-item register")
        .context("register")?;
    let start = calls.rfind("pneud \n").context("start")?;
    assert!(unregister < register && register < start);
    assert!(calls.contains("/io.github.stantonmatt.pneu.daemon"));
    let cli = s.home.join(".local/bin/pneud");
    assert_eq!(
        fs::read_link(&cli)?,
        s.home.join("Applications/Pneu.app/Contents/MacOS/pneud")
    );
    s.run("uninstall-macos.sh", &[])?;
    s.run("uninstall-macos.sh", &[])?;
    assert!(!s.home.join("Applications/Pneu.app").exists());
    assert!(cli.symlink_metadata().is_err());
    assert!(data.exists());
    let refused = s.command("uninstall-macos.sh").arg("--purge").output()?;
    assert!(!refused.status.success());
    assert!(data.exists());
    s.run("install-macos.sh", &[])?;
    s.run("uninstall-macos.sh", &["--purge"])?;
    assert!(data.exists()); // Unknown content is always retained.
    Ok(())
}

#[test]
fn failed_signing_preserves_previous_bundle_and_invalid_options_do_not_build() -> Result<()> {
    let s = Sandbox::new()?;
    let out = s.root.join("dist/macos");
    let args = ["--out", out.to_str().context("path")?];
    s.run("packaging/macos/build-app.sh", &args)?;
    fs::write(out.join("Pneu.app/marker"), "previous")?;
    assert!(
        !s.command("packaging/macos/build-app.sh")
            .args(args)
            .env("PNEU_MOCK_SIGN_FAIL", "true")
            .output()?
            .status
            .success()
    );
    assert!(out.join("Pneu.app/marker").exists());
    let before = s.calls()?;
    for args in [
        vec!["--profile", "invalid"],
        vec!["--bundle-id", "unrelated"],
        vec!["--unknown"],
    ] {
        assert!(
            !s.command("packaging/macos/build-app.sh")
                .args(args)
                .output()?
                .status
                .success()
        );
    }
    assert_eq!(s.calls()?, before);
    assert!(
        !s.command("install-macos.sh")
            .env("PNEU_MOCK_OS", "Linux")
            .output()?
            .status
            .success()
    );
    Ok(())
}

#[test]
fn unrelated_cli_is_preserved_and_approval_is_actionable() -> Result<()> {
    let s = Sandbox::new()?;
    fs::create_dir_all(s.home.join(".local/bin"))?;
    let cli = s.home.join(".local/bin/pneud");
    fs::write(&cli, "unrelated")?;
    assert!(!s.command("install-macos.sh").output()?.status.success());
    assert_eq!(fs::read_to_string(&cli)?, "unrelated");
    fs::remove_file(cli)?;
    let output = s
        .command("install-macos.sh")
        .env(
            "PNEU_MOCK_STATUS",
            "requires approval: open System Settings → General → Login Items and allow Pneu",
        )
        .output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("System Settings"));
    assert!(!s.calls()?.contains("kickstart"));
    Ok(())
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn no_argument_executable_sends_show_window_without_starting_a_second_daemon() -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let temp = tempfile::Builder::new()
        .prefix("pneu-m2-")
        .tempdir_in("/private/tmp")?;
    let root = temp.path().canonicalize()?;
    let runtime = root.join("run");
    fs::create_dir(&runtime)?;
    let listener = tokio::net::UnixListener::bind(runtime.join("pneud.sock"))?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        reader
            .get_mut()
            .write_all(b"{\"ok\":true,\"value\":null,\"more\":false}\n")
            .await?;
        Ok::<_, anyhow::Error>(line)
    });
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_pneud"))
            .env("PNEU_RUNTIME_DIR", &runtime)
            .env("PNEU_DATA_DIR", root.join("data"))
            .env("PNEU_CONFIG_DIR", root.join("config"))
            .env("PNEU_CACHE_DIR", root.join("cache"))
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let command: serde_json::Value = serde_json::from_str(&server.await??)?;
    assert_eq!(command["command"], "show_window");
    assert_eq!(command["choose_folder"], false);
    // Launch must not initialize a daemon or create its persistent state.
    assert!(!root.join("data").exists());
    assert!(!root.join("config").exists());
    assert!(!root.join("cache").exists());
    Ok(())
}

#[test]
fn missing_designer_icon_set_reports_svg_fallback() -> Result<()> {
    let s = Sandbox::new()?;
    fs::remove_dir_all(s.root.join("assets/macos/AppIcon.iconset"))?;
    fs::write(s.root.join("assets/pneu.svg"), "<svg/>")?;
    let output = s.command("packaging/macos/build-app.sh").output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("temporary icon"));
    let calls = s.calls()?;
    assert!(calls.contains("qlmanage -t -s 1024"));
    assert_eq!(
        calls
            .lines()
            .filter(|line| line.starts_with("sips -z"))
            .count(),
        10
    );
    assert!(calls.contains("sips -z 1024 1024"));
    assert!(
        s.root
            .join("target/macos/Pneu.app/Contents/Resources/AppIcon.icns")
            .is_file()
    );
    assert!(!calls.contains("heavy"));
    Ok(())
}

#[test]
fn upgrade_waits_for_the_old_launchd_process_before_replacing_the_bundle() -> Result<()> {
    let s = Sandbox::new()?;
    s.run("install-macos.sh", &[])?;
    let marker = s
        .home
        .join("Applications/Pneu.app/Contents/Resources/AppIcon.icns");
    fs::write(&marker, "old")?;
    // This process belongs only to this test. The mock bootout signals it;
    // a concurrent reaper models launchd reaping the gracefully stopped job.
    let mut old = Command::new("sleep").arg("10").spawn()?;
    let pid = old.id();
    let reaper = std::thread::spawn(move || old.wait());
    let result = s
        .command("install-macos.sh")
        .env("PNEU_MOCK_SERVICE_PID", pid.to_string())
        .env("PNEU_MOCK_OLD_MARKER", &marker)
        .env(
            "PNEU_MOCK_SERVICE_PROGRAM",
            s.home.join("Applications/Pneu.app/Contents/MacOS/pneud"),
        )
        .output();
    let stopped = reaper.join().expect("task-owned process reaper")?;
    let output = result?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !stopped.success(),
        "old process should have received bootout's SIGTERM"
    );
    assert!(
        fs::read_to_string(&marker)? != "old",
        "new bundle should replace the stopped old bundle"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "requires PNEU_TEST_BUNDLE pointing to a signed test-ID Pneu.app"]
fn bundled_cli_symlink_preserves_foundation_bundle_identity() -> Result<()> {
    use std::os::unix::fs::symlink;
    let app = PathBuf::from(std::env::var_os("PNEU_TEST_BUNDLE").context("PNEU_TEST_BUNDLE")?);
    let identifier = Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(app.join("Contents/Info.plist"))
        .output()?;
    ensure!(identifier.status.success());
    ensure!(
        matches!(
            String::from_utf8_lossy(&identifier.stdout).trim(),
            "io.github.stantonmatt.pneu.test"
                | "io.github.stantonmatt.pneu.test.pkg"
                | "io.github.stantonmatt.pneu.test.pkg2"
        ),
        "only the test bundle ID is allowed"
    );
    let temp = tempfile::tempdir()?;
    let cli = temp.path().join("pneud");
    symlink(app.join("Contents/MacOS/pneud").canonicalize()?, &cli)?;
    let output = Command::new(&cli).args(["login-item", "status"]).output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).trim().is_empty());
    Ok(())
}

#[test]
fn packaging_test_identifier_has_its_own_agent_namespace() -> Result<()> {
    let s = Sandbox::new()?;
    s.run(
        "packaging/macos/build-app.sh",
        &["--bundle-id", "io.github.stantonmatt.pneu.test.pkg"],
    )?;
    let contents = s.root.join("target/macos/Pneu.app/Contents");
    let info = fs::read_to_string(contents.join("Info.plist"))?;
    assert!(info.contains("<string>io.github.stantonmatt.pneu.test.pkg</string>"));
    let plist = fs::read_to_string(contents.join("Resources/daemon.plist.in"))?;
    assert!(
        plist.contains(
            "<key>Label</key><string>io.github.stantonmatt.pneu.test.pkg.daemon</string>"
        )
    );
    assert!(!contents.join("Library/LaunchAgents").exists());
    assert!(!s.calls()?.contains("launchctl"));
    Ok(())
}

#[test]
fn foreground_daemon_blocks_install_upgrade_uninstall_and_purge() -> Result<()> {
    let s = Sandbox::new()?;
    let first = s
        .command("install-macos.sh")
        .env("PNEU_MOCK_FOREGROUND_PID", "12345")
        .output()?;
    assert!(!first.status.success());
    assert!(!s.home.join("Applications/Pneu.app").exists());
    s.run("install-macos.sh", &[])?;
    let marker = s
        .home
        .join("Applications/Pneu.app/Contents/Resources/AppIcon.icns");
    fs::write(&marker, "old")?;
    let data = s.home.join("Library/Application Support/Pneu/keep");
    fs::create_dir_all(data.parent().context("parent")?)?;
    fs::write(&data, "private")?;
    for (script, args) in [
        ("install-macos.sh", vec![]),
        ("uninstall-macos.sh", vec![]),
        ("uninstall-macos.sh", vec!["--purge"]),
    ] {
        let output = s
            .command(script)
            .args(args)
            .env("PNEU_MOCK_FOREGROUND_PID", "12345")
            .output()?;
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("foreground pneud run") && error.contains("12345"),
            "{error}"
        );
        assert_eq!(fs::read_to_string(&marker)?, "old");
        assert_eq!(fs::read_to_string(&data)?, "private");
    }
    fs::remove_dir_all(s.home.join("Applications/Pneu.app"))?;
    assert!(
        !s.command("uninstall-macos.sh")
            .arg("--purge")
            .env("PNEU_MOCK_FOREGROUND_PID", "12345")
            .output()?
            .status
            .success()
    );
    assert!(data.exists());
    Ok(())
}

#[test]
fn process_probe_errors_fail_closed_and_upgrade_preserves_login_off() -> Result<()> {
    let s = Sandbox::new()?;
    assert!(
        !s.command("install-macos.sh")
            .env("PNEU_MOCK_PROBE_FAIL", "true")
            .output()?
            .status
            .success()
    );
    s.run("install-macos.sh", &[])?;
    fs::write(&s.trace, "")?;
    let output = s
        .command("install-macos.sh")
        .env("PNEU_MOCK_OLD_STATUS", "not registered")
        .output()?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let calls = s.calls()?;
    assert!(!calls.contains("pneud login-item register"));
    assert!(!calls.contains("pneud login-item initialize"));
    assert!(calls.contains("pneud login-item unregister"));
    assert!(calls.contains("pneud \n"));
    Ok(())
}

#[test]
fn a_colliding_launchd_label_does_not_authorize_stopping_another_executable() -> Result<()> {
    let s = Sandbox::new()?;
    s.run("install-macos.sh", &[])?;
    let marker = s
        .home
        .join("Applications/Pneu.app/Contents/Resources/AppIcon.icns");
    fs::write(&marker, "old")?;
    let mut unrelated = Command::new("sleep").arg("20").spawn()?;
    let output = s
        .command("uninstall-macos.sh")
        .env("PNEU_MOCK_SERVICE_PID", unrelated.id().to_string())
        .env("PNEU_MOCK_OLD_MARKER", &marker)
        .env("PNEU_MOCK_SERVICE_PROGRAM", "/unrelated/program")
        .output();
    let untouched = unrelated.try_wait()?.is_none();
    if untouched {
        unrelated.kill()?;
    }
    unrelated.wait()?;
    let output = output?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("different executable"));
    assert!(untouched && marker.exists());
    assert!(!s.calls()?.contains("launchctl bootout"));
    Ok(())
}

#[test]
fn purge_unlinks_only_owned_entries_and_preserves_received_files_and_symlink_targets() -> Result<()>
{
    use std::os::unix::fs::symlink;
    let s = Sandbox::new()?;
    let state = s.home.join("Library/Application Support/Pneu");
    let cache = s.home.join("Library/Caches/Pneu");
    let outside = s.home.join("received elsewhere");
    fs::create_dir_all(&outside)?;
    fs::write(outside.join("keep.txt"), "received")?;
    for spelling in ["Pneu", "pneu", "PNEU"] {
        s.run("install-macos.sh", &[])?;
        let save = state.with_file_name(spelling).join("received");
        fs::create_dir_all(&save)?;
        fs::create_dir_all(&state)?;
        fs::create_dir_all(&cache)?;
        fs::write(save.join("keep.txt"), "received")?;
        fs::write(
            state.join("config.toml"),
            format!("save_path = '{}'\n", save.display()),
        )?;
        fs::write(state.join("state.sqlite3"), "owned DB")?;
        symlink(outside.join("keep.txt"), state.join("identity.json"))?;
        symlink(&outside, state.join("run"))?;
        let cached = format!(".pneu-cache-{}-image.png", "a".repeat(32));
        symlink(outside.join("keep.txt"), cache.join(&cached))?;
        fs::create_dir_all(cache.join("unknown"))?;
        fs::write(cache.join("unknown/keep.txt"), "received")?;
        let output = s.command("uninstall-macos.sh").arg("--purge").output()?;
        ensure!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains("contains other files"));
        assert_eq!(fs::read_to_string(save.join("keep.txt"))?, "received");
        assert_eq!(fs::read_to_string(outside.join("keep.txt"))?, "received");
        assert_eq!(
            fs::read_to_string(cache.join("unknown/keep.txt"))?,
            "received"
        );
        for path in [
            state.join("state.sqlite3"),
            state.join("identity.json"),
            state.join("run"),
            cache.join(cached),
        ] {
            assert!(path.symlink_metadata().is_err(), "{}", path.display());
        }
    }
    Ok(())
}

#[test]
fn purge_removes_state_root_symlink_without_entering_received_folder() -> Result<()> {
    use std::os::unix::fs::symlink;
    let s = Sandbox::new()?;
    s.run("install-macos.sh", &[])?;
    let received = s.home.join("Downloads/Pneu");
    fs::create_dir_all(&received)?;
    // Even reserved filenames behind a directory symlink are not touched.
    fs::write(received.join("identity.json"), "received")?;
    let state = s.home.join("Library/Application Support/Pneu");
    fs::create_dir_all(state.parent().context("state parent")?)?;
    symlink(&received, &state)?;
    s.run("uninstall-macos.sh", &["--purge"])?;
    assert_eq!(
        fs::read_to_string(received.join("identity.json"))?,
        "received"
    );
    assert!(state.symlink_metadata().is_err());
    Ok(())
}

#[test]
fn upgrade_and_uninstall_preserve_unknown_bundle_content_without_config_comparisons() -> Result<()>
{
    let s = Sandbox::new()?;
    s.run("install-macos.sh", &[])?;
    let app = s.home.join("Applications/Pneu.app");
    fs::create_dir(app.join("received"))?;
    fs::write(app.join("received/keep.txt"), "received")?;
    let output = s.command("install-macos.sh").output()?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bundle contains other files"));
    assert!(app.join("Contents/MacOS/pneud").exists());
    s.run("uninstall-macos.sh", &[])?;
    assert_eq!(
        fs::read_to_string(app.join("received/keep.txt"))?,
        "received"
    );
    assert!(!app.join("Contents/MacOS/pneud").exists());
    Ok(())
}
