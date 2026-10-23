//! One recipient policy for file entry points. Captures are never rebound.
use crate::outbox::{Authority, RecipientRejection, SendTarget};
use anyhow::{Result, ensure};
use pneu_proto::Id;

pub(crate) enum Selection<'a> {
    /// Default send, or the picker candidates when offline choices are allowed.
    AllEligible { queue_offline: bool },
    /// The tray's explicitly connected-only action, regardless of preferences.
    ConnectedOnly,
    /// Naming a device explicitly permits queueing it while offline.
    Explicit(&'a [Id]),
    /// Validate the original binding, never capture its replacement.
    Captured(&'a [Authority]),
}

#[derive(Default)]
pub(crate) struct Resolution {
    pub recipients: Vec<Authority>,
    pub rejected: Vec<RecipientRejection>,
}

pub(crate) fn resolve(targets: &[SendTarget], selection: Selection<'_>) -> Result<Resolution> {
    let mut result = Resolution::default();
    let mut consider = |device: &Id, captured: Option<&Authority>| {
        if result.recipients.iter().any(|p| &p.device == device)
            || result.rejected.iter().any(|p| &p.device == device)
        {
            return;
        }
        let target = targets.iter().find(|t| &t.device == device);
        let reason = match target {
            None if captured.is_none() => Some("Device is not paired"),
            None => Some("Pairing changed"),
            Some(t) if !t.eligible => Some(t.reason.as_deref().unwrap_or("Pairing is unavailable")),
            Some(t) if t.generation == 0 || t.fingerprint.is_none() => {
                Some("Pairing is unavailable")
            }
            Some(t)
                if captured.is_some_and(|a| {
                    a.generation != t.generation || Some(&a.fingerprint) != t.fingerprint.as_ref()
                }) =>
            {
                Some("Pairing changed")
            }
            _ => None,
        };
        if let Some(reason) = reason {
            result.rejected.push(RecipientRejection {
                device: device.clone(),
                reason: reason.into(),
            });
        } else if let Some(t) = target {
            result
                .recipients
                .push(captured.cloned().unwrap_or_else(|| Authority {
                    device: t.device.clone(),
                    generation: t.generation,
                    fingerprint: t.fingerprint.clone().expect("eligible binding"),
                }));
        }
    };
    match selection {
        Selection::AllEligible { queue_offline } => {
            for t in targets
                .iter()
                .filter(|t| t.eligible && (queue_offline || !t.would_queue))
            {
                consider(&t.device, None);
            }
        }
        Selection::ConnectedOnly => {
            for t in targets.iter().filter(|t| t.eligible && !t.would_queue) {
                consider(&t.device, None);
            }
        }
        Selection::Explicit(ids) => {
            ensure!(
                !ids.is_empty() && ids.len() <= 128,
                "choose 1 to 128 devices"
            );
            for id in ids {
                consider(id, None);
            }
        }
        Selection::Captured(entries) => {
            ensure!(entries.len() <= 128, "choose up to 128 devices");
            for a in entries {
                consider(&a.device, Some(a));
            }
        }
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pneu_proto::Hash;

    #[test]
    fn modes_share_eligibility_but_keep_explicit_offline_and_captured_authority() -> Result<()> {
        let mut targets: Vec<_> = (1..=4)
            .map(|n| SendTarget {
                device: Id::from_bytes([n; 16]),
                name: format!("Device {n}"),
                generation: n as u64,
                fingerprint: Some(Hash::new("ab".repeat(32)).unwrap()),
                eligible: n != 3,
                would_queue: n == 2,
                reason: (n == 3).then(|| "Update this device to receive files".into()),
            })
            .collect();
        targets[3].fingerprint = None;
        for (queue_offline, expected) in [(false, vec![1]), (true, vec![1, 2])] {
            let result = resolve(&targets, Selection::AllEligible { queue_offline })?;
            assert_eq!(
                result
                    .recipients
                    .iter()
                    .map(|a| a.device.clone())
                    .collect::<Vec<_>>(),
                expected
                    .into_iter()
                    .map(|n| Id::from_bytes([n; 16]))
                    .collect::<Vec<_>>()
            );
        }
        let connected = resolve(&targets, Selection::ConnectedOnly)?.recipients;
        assert_eq!(connected.len(), 1);
        let ids = [1, 2, 3, 4, 5, 2].map(|n| Id::from_bytes([n; 16]));
        let explicit = resolve(&targets, Selection::Explicit(&ids))?;
        assert_eq!(explicit.recipients.len(), 2);
        assert_eq!(
            explicit
                .rejected
                .iter()
                .map(|r| r.reason.as_str())
                .collect::<Vec<_>>(),
            [
                "Update this device to receive files",
                "Pairing is unavailable",
                "Device is not paired"
            ]
        );
        let captured = explicit.recipients;
        assert_eq!(
            resolve(&targets, Selection::Captured(&captured))?.recipients,
            captured
        );
        targets[0].generation += 1;
        targets[1].fingerprint = Some(Hash::new("cd".repeat(32))?);
        let stale = resolve(&targets, Selection::Captured(&captured))?;
        assert!(stale.recipients.is_empty());
        assert_eq!(stale.rejected.len(), 2);
        assert!(stale.rejected.iter().all(|r| r.reason == "Pairing changed"));
        assert!(resolve(&targets, Selection::Explicit(&[])).is_err());
        assert!(resolve(&targets, Selection::Explicit(&vec![ids[0].clone(); 129])).is_err());
        Ok(())
    }
}
