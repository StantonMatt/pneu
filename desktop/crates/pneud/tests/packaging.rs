use anyhow::Result;
use std::process::Command;

#[test]
fn reinstall_reenables_graphical_session_unit_and_restarts_only_in_session() -> Result<()> {
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
        // Source the actual installer with shell functions replacing all side
        // effects. No build, install or real user-service operation occurs.
        let output = Command::new("bash")
            .args([
                "-c",
                r#"
heavy() { printf 'build\n' >> "$PNEU_INSTALL_TRACE"; }
install() { printf 'install\n' >> "$PNEU_INSTALL_TRACE"; }
systemctl() {
    printf '%s\n' "$*" >> "$PNEU_INSTALL_TRACE"
    if [[ "$*" == '--user is-active --quiet graphical-session.target' ]]; then
        [[ "$PNEU_GRAPHICAL_ACTIVE" == true ]]
    fi
}
source "$1"
"#,
                "test-installer",
            ])
            .arg(&installer)
            .env("PNEU_INSTALL_TRACE", &trace)
            .env("PNEU_GRAPHICAL_ACTIVE", active.to_string())
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let calls = std::fs::read_to_string(trace)?;
        let mut expected = "build\ninstall\ninstall\ninstall\ninstall\ninstall\ninstall\ninstall\ninstall\ninstall\ninstall\n--user daemon-reload\n--user reenable pneud.service\n--user is-active --quiet graphical-session.target\n".to_owned();
        if active {
            expected.push_str("--user restart pneud.service\n");
        }
        assert_eq!(calls, expected);
    }
    Ok(())
}

#[test]
fn dolphin_service_menu_uses_absolute_path_cli_and_executable_kf6_install() {
    let menu = include_str!("../../../packaging/pneu-send.desktop");
    assert!(menu.contains("Type=Service"));
    assert!(menu.contains("MimeType=application/octet-stream;"));
    assert!(menu.contains("Name=Send with Pneu"));
    assert!(menu.contains("Exec=pneud send %F"));
    let installer = include_str!("../../../install.sh");
    assert!(installer.contains("install -Dm755 packaging/pneu-send.desktop \"$HOME/.local/share/kio/servicemenus/pneu-send.desktop\""));
}
