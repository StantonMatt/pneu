//! Formatting for daemon diagnostics. Callers must supply content-free messages.
use pneu_proto::Id;

pub(crate) fn error_line(device: Option<&Id>, message: &str) -> String {
    // journald timestamps Linux stderr; the macOS LaunchAgent appends it verbatim.
    #[cfg(target_os = "macos")]
    let timestamp = format!(
        "{} ",
        chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    );
    #[cfg(not(target_os = "macos"))]
    let timestamp = "";
    match device {
        Some(device) => format!("pneud: {timestamp}peer={device} {message}"),
        None => format!("pneud: {timestamp}{message}"),
    }
}
