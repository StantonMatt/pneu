//! Platform-independent admission and presentation of camera results. Trust,
//! expiry, own-device and existing-pair checks belong to the ordinary Join IPC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Permission {
    NotDetermined,
    Restricted,
    Denied,
    Authorized,
}
impl Permission {
    pub fn from_native(status: i32) -> Self {
        match status {
            0 => Self::NotDetermined,
            3 => Self::Authorized,
            1 => Self::Restricted,
            _ => Self::Denied,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum State {
    Unavailable,
    Requesting,
    Denied,
    Scanning,
}
impl State {
    pub fn new(available: bool, permission: Permission) -> Self {
        if !available {
            return Self::Unavailable;
        }
        match permission {
            Permission::NotDetermined => Self::Requesting,
            Permission::Denied | Permission::Restricted => Self::Denied,
            Permission::Authorized => Self::Scanning,
        }
    }
}

/// Only select the QR's destination here. Incomplete pairing links still reach
/// Join so scanned and pasted links produce exactly the same error table.
pub(super) fn pairing_payload(text: &str) -> Option<&str> {
    let text = text.trim();
    (text.len() <= 8193 && (text == "pneu://pair" || text.starts_with("pneu://pair?")))
        .then_some(text)
}

/// Every open/close invalidates callbacks from older permission requests and
/// sessions. A permission response after closing must never start a camera.
#[derive(Default)]
pub(super) struct Generation {
    value: u64,
    active: bool,
}
impl Generation {
    pub fn open(&mut self) -> u64 {
        self.value = self.value.wrapping_add(1);
        self.active = true;
        self.value
    }
    pub fn close(&mut self) {
        self.active = false;
    }
    pub fn accepts(&self, value: u64) -> bool {
        self.active && self.value == value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_and_every_authorization_state() {
        for (raw, permission, state) in [
            (0, Permission::NotDetermined, State::Requesting),
            (1, Permission::Restricted, State::Denied),
            (2, Permission::Denied, State::Denied),
            (3, Permission::Authorized, State::Scanning),
            (99, Permission::Denied, State::Denied),
        ] {
            assert_eq!(Permission::from_native(raw), permission);
            assert_eq!(State::new(true, permission), state);
            assert_eq!(State::new(false, permission), State::Unavailable);
        }
    }

    #[test]
    fn filters_other_qr_destinations_without_validating_pairing_authority() {
        for text in [
            "https://example.com",
            "pneu://send?x=y",
            "pneu://pairing?x=y",
            "",
            "PNEU://pair?x=y",
        ] {
            assert_eq!(pairing_payload(text), None);
        }
        // The ordinary Join command explains damaged, expired, used and own codes.
        assert_eq!(
            pairing_payload(" pneu://pair?broken \n"),
            Some("pneu://pair?broken")
        );
        assert_eq!(pairing_payload("pneu://pair"), Some("pneu://pair"));
        assert!(pairing_payload(&format!("pneu://pair?{}", "x".repeat(8193))).is_none());
    }

    #[test]
    fn close_reopen_and_first_result_fence_late_callbacks() {
        let mut generation = Generation::default();
        let first = generation.open();
        assert!(generation.accepts(first));
        generation.close();
        assert!(!generation.accepts(first));
        let second = generation.open();
        assert!(!generation.accepts(first));
        assert!(generation.accepts(second));
        generation.close();
        assert!(!generation.accepts(second));
    }
}
