//! One-shot activation credentials belong to a notification action, never the daemon.
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const TOKEN_TTL: Duration = Duration::from_secs(30);
const TOKEN_LIMIT: usize = 64;

#[derive(Default)]
pub(crate) struct NotificationTokens {
    // Only credentials awaiting an action live here, never notification IDs.
    entries: HashMap<u32, (Instant, String)>,
}
impl NotificationTokens {
    fn expire(&mut self, now: Instant) {
        self.entries.retain(|_, (received, _)| {
            now.checked_duration_since(*received)
                .is_some_and(|age| age < TOKEN_TTL)
        });
    }
    pub fn receive(&mut self, id: u32, token: String, now: Instant) {
        self.expire(now);
        if token.is_empty() || token.len() > 4096 {
            return;
        }
        if self.entries.len() >= TOKEN_LIMIT
            && !self.entries.contains_key(&id)
            && let Some(old) = self
                .entries
                .iter()
                .min_by_key(|(_, (received, _))| *received)
                .map(|(id, _)| *id)
        {
            self.entries.remove(&old);
        }
        self.entries.insert(id, (now, token));
    }
    pub fn take(&mut self, id: u32, now: Instant) -> Option<String> {
        self.expire(now);
        self.entries.remove(&id).map(|(_, token)| token)
    }
    pub fn remove(&mut self, id: u32) {
        self.entries.remove(&id);
    }
}

pub(crate) fn open_uri_options<'a>(
    handle: &'a str,
    token: Option<&'a str>,
) -> HashMap<&'static str, zbus::zvariant::Value<'a>> {
    let mut options = HashMap::from([("handle_token", zbus::zvariant::Value::from(handle))]);
    if let Some(token) = token {
        options.insert("activation_token", zbus::zvariant::Value::from(token));
    }
    options
}
pub(crate) fn command(
    program: impl AsRef<std::ffi::OsStr>,
    token: Option<&str>,
) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    // A long-lived daemon may have inherited its own, already-used startup token.
    command
        .env_remove("XDG_ACTIVATION_TOKEN")
        .env_remove("DESKTOP_STARTUP_ID");
    if let Some(token) = token {
        command
            .env("XDG_ACTIVATION_TOKEN", token)
            .env("DESKTOP_STARTUP_ID", token);
    }
    command
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pending_tokens_are_scoped_and_consumed_once_without_registration() {
        let now = Instant::now();
        let mut tokens = NotificationTokens::default();
        tokens.receive(1, "first".into(), now);
        assert_eq!(tokens.take(2, now), None);
        assert_eq!(tokens.take(1, now).as_deref(), Some("first"));
        assert_eq!(tokens.take(1, now), None);
        tokens.receive(1, "second".into(), now);
        assert_eq!(tokens.take(1, now).as_deref(), Some("second"));
        assert_eq!(tokens.take(1, now), None);
        tokens.receive(2, "closed".into(), now);
        tokens.remove(2);
        assert_eq!(tokens.take(2, now), None);
        tokens.receive(3, String::new(), now);
        tokens.receive(4, "x".repeat(4097), now);
        assert!(tokens.entries.is_empty());
    }
    #[test]
    fn expired_tokens_are_dropped_before_receiving_or_taking_other_ids() {
        let now = Instant::now();
        let mut tokens = NotificationTokens::default();
        tokens.receive(1, "stale".into(), now);
        tokens.receive(2, "fresh".into(), now + TOKEN_TTL);
        assert!(!tokens.entries.contains_key(&1));
        assert_eq!(tokens.take(1, now + TOKEN_TTL), None);
        assert_eq!(tokens.take(2, now + TOKEN_TTL).as_deref(), Some("fresh"));
        tokens.receive(3, "also stale".into(), now);
        assert_eq!(tokens.take(99, now + TOKEN_TTL), None);
        assert!(tokens.entries.is_empty());
        assert_eq!(tokens.take(3, now + TOKEN_TTL), None);
    }
    #[test]
    fn pending_tokens_are_bounded_and_evict_the_oldest_received_token() {
        let now = Instant::now();
        let mut tokens = NotificationTokens::default();
        for id in 0..=TOKEN_LIMIT as u32 {
            tokens.receive(id, id.to_string(), now + Duration::from_millis(id.into()));
            assert!(tokens.entries.len() <= TOKEN_LIMIT);
        }
        assert_eq!(tokens.take(0, now + Duration::from_secs(1)), None);
        assert_eq!(
            tokens.take(1, now + Duration::from_secs(1)).as_deref(),
            Some("1")
        );
    }
    #[test]
    fn launch_options_and_child_environment_use_only_the_supplied_token() {
        let options = open_uri_options("handle", Some("activation"));
        assert_eq!(
            <&str>::try_from(&options["activation_token"]).unwrap(),
            "activation"
        );
        assert!(!super::open_uri_options("handle", None).contains_key("activation_token"));
        for token in [None, Some("activation")] {
            let cmd = command("xdg-open", token);
            let env: HashMap<_, _> = cmd.as_std().get_envs().collect();
            for key in ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"] {
                assert_eq!(
                    env[std::ffi::OsStr::new(key)],
                    token.map(std::ffi::OsStr::new)
                );
            }
        }
    }
}
