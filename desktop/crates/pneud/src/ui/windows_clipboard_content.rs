//! Pure Windows clipboard policy and bounded interchange conversion.
#![cfg_attr(not(windows), allow(dead_code))]
use super::clipboard::{Content, MAX_IMAGE};
use anyhow::{Result, ensure};

pub(super) const INSTALL: &str = "io.github.stantonmatt.pneu.install";
pub(super) const EXCLUDE: &str = "ExcludeClipboardContentFromMonitorProcessing";
pub(super) const IGNORE: &str = "Clipboard Viewer Ignore";
pub(super) const HISTORY: &str = "CanIncludeInClipboardHistory";
pub(super) const CLOUD: &str = "CanUploadToCloudClipboard";
pub(super) const MAX_DIB: usize = 64 * 1024 * 1024 + 2048;
pub(super) const OUTPUT_CAP: usize = MAX_IMAGE * 5 + crate::store::history::MAX_TEXT * 6 + 4096;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Classification {
    Sensitive,
    Origin,
    Local,
}
pub(super) fn classify(formats: &[(String, Option<u32>)]) -> Classification {
    if formats.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case(EXCLUDE)
            || name.eq_ignore_ascii_case(IGNORE)
            || (name.eq_ignore_ascii_case(HISTORY) && *value != Some(1))
    }) {
        // Missing/malformed history data fails closed too.
        Classification::Sensitive
    } else if formats
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case(INSTALL))
    {
        Classification::Origin
    } else {
        Classification::Local
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct Snapshot {
    #[serde(default)]
    pub status: SnapshotStatus,
    pub marker: Option<String>,
    pub content: Option<Content>,
    #[serde(default)]
    pub oversized: Option<[u8; 32]>,
}
impl Snapshot {
    pub fn empty() -> Self {
        Self {
            status: SnapshotStatus::Ready,
            marker: None,
            content: None,
            oversized: None,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) enum SnapshotStatus {
    #[default]
    Ready,
    Changed,
    Sensitive,
}
pub(super) fn parse_output(bytes: Vec<u8>) -> Result<Snapshot> {
    ensure!(bytes.len() <= OUTPUT_CAP, "clipboard reader output limit");
    let result: Snapshot = serde_json::from_slice(&bytes)?;
    ensure!(
        result.status == SnapshotStatus::Ready
            || (result.marker.is_none() && result.content.is_none() && result.oversized.is_none()),
        "unavailable clipboard snapshot contains data"
    );
    ensure!(
        result
            .marker
            .as_ref()
            .is_none_or(|m| super::pasteboard_reader::ticket(m.as_bytes()).is_some()),
        "invalid clipboard marker"
    );
    let (text, png) = match &result.content {
        Some(Content::Text(t)) => (Some(t), None),
        Some(Content::Image(p)) => (None, Some(p)),
        Some(Content::Formats(t, p)) => (Some(t), Some(p)),
        None => (None, None),
    };
    ensure!(text.is_none_or(|t| valid_text(t)), "invalid clipboard text");
    ensure!(
        png.is_none_or(|p| p.len() <= MAX_IMAGE),
        "clipboard PNG limit"
    );
    Ok(result)
}
pub(super) fn valid_text(text: &str) -> bool {
    text.len() <= crate::store::history::MAX_TEXT && !text.contains('\0')
}
pub(super) fn combine(text: Option<String>, png: Option<Vec<u8>>) -> Option<Content> {
    match (text, png) {
        (Some(t), Some(p)) => Some(Content::Formats(t, p)),
        (Some(t), None) => Some(Content::Text(t)),
        (None, Some(p)) => Some(Content::Image(p)),
        _ => None,
    }
}
pub(super) fn png(image: &image::DynamicImage) -> Result<Vec<u8>> {
    let mut output = std::io::Cursor::new(Vec::new());
    image.write_to(&mut output, image::ImageFormat::Png)?;
    ensure!(
        output.get_ref().len() <= MAX_IMAGE,
        crate::actions::ImageTooLarge
    );
    Ok(output.into_inner())
}
pub(super) fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        dib.len() >= 40 && dib.len() <= MAX_DIB,
        "clipboard DIB size"
    );
    let u32_at = |n| u32::from_le_bytes(dib[n..n + 4].try_into().unwrap());
    let header = u32_at(0) as usize;
    ensure!(
        matches!(header, 40 | 108 | 124) && dib.len() >= header,
        "unsupported DIB header"
    );
    let bits = u16::from_le_bytes(dib[14..16].try_into().unwrap());
    ensure!(
        matches!(bits, 1 | 4 | 8 | 16 | 24 | 32),
        "unsupported DIB depth"
    );
    let compression = u32_at(16);
    ensure!(
        compression == 0 || compression == 3,
        "unsupported DIB compression"
    );
    let colors = u32_at(32) as usize;
    let colors = if colors == 0 && bits <= 8 {
        1usize << bits
    } else {
        colors
    };
    ensure!(colors <= 256, "DIB palette limit");
    let offset = header
        + if header == 40 && compression == 3 {
            12
        } else {
            0
        }
        + colors * 4;
    ensure!(offset <= dib.len(), "truncated DIB palette or masks");
    // The existing BMP decoder validates stride, dimensions and allocation
    // limits. A DIB differs only in its missing 14-byte file header.
    let mut bmp = Vec::with_capacity(14 + dib.len());
    bmp.extend_from_slice(b"BM");
    bmp.extend_from_slice(&((14 + dib.len()) as u32).to_le_bytes());
    bmp.extend_from_slice(&[0; 4]);
    bmp.extend_from_slice(&((14 + offset) as u32).to_le_bytes());
    bmp.extend_from_slice(dib);
    png(&crate::actions::decode_image(&bmp)?)
}
pub(super) fn image_to_dib(image: &image::DynamicImage) -> Result<Vec<u8>> {
    let rgba = image.to_rgba8();
    ensure!(rgba.len() <= MAX_DIB - 124, "clipboard DIB limit");
    let mut dib = vec![0u8; 124];
    for (offset, value) in [
        (0, 124),
        (4, rgba.width()),
        (8, (-(rgba.height() as i32)) as u32),
        (16, 3),
        (20, rgba.len() as u32),
        (40, 0xff0000),
        (44, 0xff00),
        (48, 0xff),
        (52, 0xff000000),
        (56, 0x73524742),
    ] {
        dib[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    dib[12..14].copy_from_slice(&1u16.to_le_bytes());
    dib[14..16].copy_from_slice(&32u16.to_le_bytes());
    for pixel in rgba.pixels() {
        dib.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    Ok(dib)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clipboard_privacy_and_origin() {
        // RegisterClipboardFormat's name comparison is case-insensitive.
        assert_eq!(
            classify(&[(EXCLUDE.to_ascii_lowercase(), None)]),
            Classification::Sensitive
        );
        assert_eq!(
            classify(&[(HISTORY.to_ascii_uppercase(), Some(0))]),
            Classification::Sensitive
        );
        assert_eq!(
            classify(&[(INSTALL.to_ascii_uppercase(), None)]),
            Classification::Origin
        );
        for name in [EXCLUDE, IGNORE] {
            assert_eq!(
                classify(&[(INSTALL.into(), None), (name.into(), None)]),
                Classification::Sensitive
            );
        }
        for value in [None, Some(0), Some(2)] {
            assert_eq!(
                classify(&[(HISTORY.into(), value)]),
                Classification::Sensitive
            );
        }
        assert_eq!(
            classify(&[(HISTORY.into(), Some(1)), (CLOUD.into(), Some(0))]),
            Classification::Local
        );
        assert_eq!(classify(&[(INSTALL.into(), None)]), Classification::Origin);
    }
    #[test]
    fn clipboard_dib_v5_and_bottom_up_dib() -> Result<()> {
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_fn(2, 2, |x, y| {
            image::Rgba([x as u8 * 100, y as u8 * 100, 40, 255])
        }));
        let dib = image_to_dib(&image)?;
        assert_eq!(
            image::load_from_memory(&dib_to_png(&dib)?)?.to_rgba8(),
            image.to_rgba8()
        );
        let mut old = dib[..40].to_vec();
        old[..4].copy_from_slice(&40u32.to_le_bytes());
        old[8..12].copy_from_slice(&2i32.to_le_bytes());
        old[16..20].fill(0);
        old.extend_from_slice(&dib[132..140]);
        old.extend_from_slice(&dib[124..132]);
        assert_eq!(
            image::load_from_memory(&dib_to_png(&old)?)?.to_rgb8(),
            image.to_rgb8()
        );
        for truncated in [&dib[..39], &dib[..123], &old[..41]] {
            assert!(dib_to_png(truncated).is_err());
        }
        old[4..8].copy_from_slice(&i32::MAX.to_le_bytes());
        assert!(dib_to_png(&old).is_err());
        Ok(())
    }
    #[test]
    fn clipboard_helper_output_limits() -> Result<()> {
        let output = Snapshot {
            oversized: None,
            status: SnapshotStatus::Ready,
            marker: Some(format!("{:032x}", 42)),
            content: Some(Content::Text("🦀".into())),
        };
        assert!(parse_output(serde_json::to_vec(&output)?).is_ok());
        for content in [
            Content::Text("a\0b".into()),
            Content::Text("a".repeat(crate::store::history::MAX_TEXT + 1)),
            Content::Image(vec![0; MAX_IMAGE + 1]),
        ] {
            assert!(
                parse_output(serde_json::to_vec(&Snapshot {
                    oversized: None,
                    status: SnapshotStatus::Ready,
                    marker: None,
                    content: Some(content)
                })?)
                .is_err()
            );
        }
        assert!(parse_output(b"{} trailing".to_vec()).is_err());
        assert!(
            parse_output(serde_json::to_vec(&Snapshot {
                oversized: None,
                status: SnapshotStatus::Ready,
                marker: Some("bad".into()),
                content: None
            })?)
            .is_err()
        );
        assert!(parse_output(vec![0; OUTPUT_CAP + 1]).is_err());
        Ok(())
    }
}
