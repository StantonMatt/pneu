#![cfg(target_os = "linux")]
use anyhow::Result;
use std::process::Command;

#[test]
fn source_reinstall_builds_then_reenables_and_restarts_only_in_session() -> Result<()> {
    let installer = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../install.sh")
        .canonicalize()?;
    let unit = include_str!("../../../packaging/pneud.service");
    assert!(unit.contains("WantedBy=graphical-session.target"));
    assert!(unit.contains("PartOf=graphical-session.target"));
    assert!(!unit.contains("default.target"));
    for active in [false, true] {
        let dir = tempfile::tempdir()?;
        let trace = dir.path().join("commands");
        // Source the real installer with every mutation replaced by a shell
        // function. No build/install or host user-service operation occurs.
        let output = Command::new("bash")
            .args([
                "-c",
                r#"
heavy() { printf 'build\n' >> "$PNEU_INSTALL_TRACE"; }
install() { printf 'install %s\n' "$*" >> "$PNEU_INSTALL_TRACE"; }
systemctl() {
    printf '%s\n' "$*" >> "$PNEU_INSTALL_TRACE"
    if [[ "$*" == '--user is-active --quiet graphical-session.target' ]]; then
        [[ "$PNEU_GRAPHICAL_ACTIVE" == true ]]
    fi
}
set --
source "$PNEU_INSTALLER"
"#,
            ])
            .env("HOME", dir.path())
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env("PNEU_INSTALLER", &installer)
            .env("PNEU_INSTALL_TRACE", &trace)
            .env("PNEU_GRAPHICAL_ACTIVE", active.to_string())
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = std::fs::read_to_string(trace)?;
        assert!(calls.starts_with("build\ninstall -Dm755 target/release/pneud "));
        assert_eq!(
            calls
                .lines()
                .filter(|line| line.starts_with("install "))
                .count(),
            5
        );
        assert!(calls.contains(&format!(
            "{}/.config/systemd/user/pneud.service",
            dir.path().display()
        )));
        let mut expected = "--user daemon-reload\n--user reenable pneud.service\n--user is-active --quiet graphical-session.target\n".to_owned();
        if active {
            expected.push_str("--user restart pneud.service\n");
        }
        assert!(calls.ends_with(&expected));
    }
    Ok(())
}

#[test]
fn binary_installer_reuses_installation_without_a_build() {
    let binary = include_str!("../../../packaging/linux/install-binary.sh");
    let source = include_str!("../../../install.sh");
    for script in [binary, source] {
        assert!(script.contains("install-user.sh"));
        assert!(script.contains("pneu_install_user"));
        assert!(script.contains("pneu_uninstall_user"));
        assert!(script.contains("--uninstall"));
    }
    assert!(!binary.contains("cargo"));
}

#[test]
fn desktop_integration_is_visible_and_uses_kf6_locations() {
    let desktop = include_str!("../../../packaging/pneu.desktop");
    assert!(desktop.contains("Exec=pneud window"));
    assert!(!desktop.contains("NoDisplay=true"));
    let menu = include_str!("../../../packaging/pneu-send.desktop");
    assert!(menu.contains("Type=Service"));
    assert!(menu.contains("Exec=pneud send %F"));
    let installer = include_str!("../../../packaging/linux/install-user.sh");
    assert!(installer.contains("install -Dm755 \"$root/packaging/pneu-send.desktop\" \"$data/kio/servicemenus/pneu-send.desktop\""));
}

#[test]
fn deb_accepts_both_qt_package_names_with_the_derived_minimum() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir()?;
    let stub = dir.path().join("dpkg-shlibdeps");
    std::fs::write(
        &stub,
        "#!/bin/sh\nprintf '%s\\n' 'shlibs:Depends=libc6 (>= 2.34), libqt6gui6t64 (>= 6.4.2), libqt6widgets6t64 (>= 6.4.2)'\n",
    )?;
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755))?;
    let script = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/linux/build-packages.sh");
    // An ordinary ELF fixture exercises real package generation and control
    // parsing without installing anything or requiring a Qt-linked fixture.
    let output = Command::new("bash")
        .arg(script)
        .args(["/bin/true", "1.2.3"])
        .arg(dir.path().join("out"))
        .env("SOURCE_DATE_EPOCH", "1700000000")
        .env(
            "PATH",
            format!("{}:{}", dir.path().display(), std::env::var("PATH")?),
        )
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let control = Command::new("dpkg-deb")
        .arg("--field")
        .arg(dir.path().join("out/pneu_1.2.3_amd64.deb"))
        .arg("Depends")
        .output()?;
    assert!(control.status.success());
    let depends = String::from_utf8(control.stdout)?;
    for library in ["gui", "widgets"] {
        assert!(
            depends.contains(&format!(
                "libqt6{library}6t64 (>= 6.4.2) | libqt6{library}6 (>= 6.4.2)"
            )),
            "{depends}"
        );
    }
    assert!(depends.contains("libc6 (>= 2.34)"));
    Ok(())
}
