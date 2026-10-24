//! Geometry from the rendered, clipped Slint list. Unresolved locations fail
//! closed; coordinates never imply an all-devices fallback.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Geometry {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scroll: f64,
    pub row_height: f64,
}
pub(crate) fn hit(geometry: Geometry, point: (f64, f64), enabled: &[bool]) -> Option<usize> {
    let Geometry {
        x,
        y,
        width,
        height,
        scroll,
        row_height,
    } = geometry;
    if ![x, y, width, height, scroll, row_height, point.0, point.1]
        .iter()
        .all(|v| v.is_finite())
        || width <= 0.0
        || height <= 0.0
        || row_height <= 0.0
    {
        return None;
    }
    let (px, py) = (point.0 - x, point.1 - y);
    if px < 0.0 || py < 0.0 || px >= width || py >= height {
        return None;
    }
    let row = ((py - scroll) / row_height).floor();
    if row < 0.0 {
        return None;
    }
    let index = row as usize;
    enabled
        .get(index)
        .copied()
        .unwrap_or(false)
        .then_some(index)
}
pub(crate) fn has_connected_target<'a>(
    targets: impl Iterator<Item = &'a str>,
    connected: impl Fn(&str) -> bool,
) -> bool {
    targets.into_iter().any(connected)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_connected_drop_resolution_excludes_eligible_offline_devices() -> anyhow::Result<()> {
        use crate::ui::window::contract::{SendTarget, resolve_files};
        let targets = [
            SendTarget {
                device: "11".repeat(16),
                eligible: true,
                generation: 1,
                fingerprint: Some("22".repeat(32)),
                would_queue: false,
                ..Default::default()
            },
            SendTarget {
                device: "33".repeat(16),
                eligible: true,
                generation: 1,
                fingerprint: Some("44".repeat(32)),
                would_queue: true,
                ..Default::default()
            },
        ];
        let captured = resolve_files(&targets, crate::file_recipients::Selection::ConnectedOnly)?;
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].device, targets[0].device);
        assert!(
            resolve_files(
                &targets[1..],
                crate::file_recipients::Selection::ConnectedOnly
            )?
            .is_empty()
        );
        Ok(())
    }
    #[test]
    fn all_device_row_requires_a_captured_recipient_still_connected() {
        assert!(!has_connected_target(["offline"].into_iter(), |_| false));
        assert!(has_connected_target(
            ["online", "offline"].into_iter(),
            |id| id == "online"
        ));
        // Captured during a drag, then disconnected before the native drop.
        let captured = ["online"];
        assert!(!has_connected_target(captured.into_iter(), |_| false));
        // Newly connected devices were not captured and cannot authorize it.
        assert!(!has_connected_target(captured.into_iter(), |id| id == "new"));
    }
    #[test]
    fn hit_testing_uses_clipped_scrolled_rows_and_rejects_unresolved_drops() {
        let mut g = Geometry {
            x: 16.0,
            y: 32.0,
            width: 368.0,
            height: 168.0,
            scroll: 0.0,
            row_height: 56.0,
        };
        assert_eq!(hit(g, (16.0, 32.0), &[true, true, false]), Some(0));
        assert_eq!(hit(g, (32.0, 88.0), &[true, true, false]), Some(1));
        for point in [
            (0.0, 0.0),
            (384.0, 32.0),
            (32.0, 200.0),
            (32.0, 144.0),
            (f64::NAN, 32.0),
        ] {
            assert_eq!(hit(g, point, &[true, true, false]), None);
        }
        g.scroll = -56.0;
        assert_eq!(hit(g, (32.0, 32.0), &[true, true, true]), Some(1));
        assert_eq!(hit(g, (32.0, 199.0), &[true, true, true]), None);
        g.row_height = 0.0;
        assert_eq!(hit(g, (32.0, 32.0), &[true]), None);
    }
}
