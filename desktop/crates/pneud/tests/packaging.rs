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
function /home/mjstanton/.local/bin/heavy() { printf 'build\n' >> "$PNEU_INSTALL_TRACE"; }
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
        let mut expected = "build\ninstall\ninstall\ninstall\ninstall\ninstall\ninstall\ninstall\ninstall\ninstall\n--user daemon-reload\n--user reenable pneud.service\n--user is-active --quiet graphical-session.target\n".to_owned();
        if active {
            expected.push_str("--user restart pneud.service\n");
        }
        assert_eq!(calls, expected);
    }
    Ok(())
}
