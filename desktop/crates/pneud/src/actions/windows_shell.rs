//! Shell calls stay in the requesting process to retain its foreground rights.
use crate::{
    actions::{FileUnavailable, ReceivedFile},
    platform::fs as platform_fs,
    ui::portal::Interaction,
};
use anyhow::{Context, Result};
use std::{
    collections::BTreeMap,
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    ptr::null_mut,
};
use windows::{
    Win32::{
        System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree, CoUninitialize},
        UI::Shell::{
            Common::ITEMIDLIST, ILFindLastID, SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC,
            SHELLEXECUTEINFOW, SHOpenFolderAndSelectItems, SHParseDisplayName, ShellExecuteExW,
        },
    },
    core::PCWSTR,
};

async fn sta<T: Send + 'static>(
    prepare: impl FnOnce() -> Result<T> + Send + 'static,
    effect: impl FnOnce(T) -> Result<()> + Send + 'static,
) -> Result<Interaction> {
    let (send, receive) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("pneu-shell".into())
        .spawn(move || {
            struct Apartment;
            impl Drop for Apartment {
                fn drop(&mut self) {
                    unsafe { CoUninitialize() }
                }
            }
            let result = (|| {
                unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()? };
                let _apartment = Apartment;
                let prepared = prepare()?;
                // Cancellation while preparing must prevent Shell issuance.
                if send.is_closed() {
                    return Ok(());
                }
                effect(prepared)
            })();
            let _ = send.send(result);
        })?;
    receive.await??;
    Ok(Box::pin(async { Ok(()) }))
}

fn checked_path(file: &ReceivedFile) -> Result<&Path> {
    let check = || -> Result<()> {
        let named = platform_fs::open_regular_file(file.path())?;
        anyhow::ensure!(
            platform_fs::identity(&named)? == platform_fs::identity(&file.file)?,
            "received file identity changed"
        );
        Ok(())
    };
    check().map_err(|_| FileUnavailable)?;
    Ok(file.path())
}

pub(super) async fn open_received(file: ReceivedFile) -> Result<Interaction> {
    sta(
        move || {
            checked_path(&file)?;
            Ok(file)
        },
        |file| execute(file.path()),
    )
    .await
}

pub(super) async fn open_url(url: String) -> Result<Interaction> {
    let url = crate::ui::notifications::single_url(&url).context("invalid notification URL")?;
    sta(|| Ok(()), move |()| execute(Path::new(&url))).await
}

pub(super) async fn open_folder(path: PathBuf) -> Result<Interaction> {
    sta(
        move || platform_fs::Folder::open(&path, false)?.path(),
        |path| execute(&path),
    )
    .await
}

fn wide(path: &Path) -> Result<Vec<u16>> {
    let mut text: Vec<u16> = path.as_os_str().encode_wide().collect();
    anyhow::ensure!(!text.contains(&0), "path contains a null character");
    text.push(0);
    Ok(text)
}

fn execute(path: &Path) -> Result<()> {
    let path = wide(path)?;
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        // Finish DDE/delegate execution before the STA exits. A null verb uses
        // the user's default association; report failures through existing UI.
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpFile: PCWSTR(path.as_ptr()),
        nShow: 1, // SW_SHOWNORMAL
        ..Default::default()
    };
    unsafe { ShellExecuteExW(&mut info)? };
    Ok(())
}

struct Pidl(*mut ITEMIDLIST);
impl Pidl {
    fn parse(path: &Path) -> Result<Self> {
        let path = wide(path)?;
        let mut pidl = Self(null_mut());
        unsafe { SHParseDisplayName(PCWSTR(path.as_ptr()), None, &mut pidl.0, 0, None)? };
        anyhow::ensure!(!pidl.0.is_null(), "Shell returned no item");
        Ok(pidl)
    }
}
impl Drop for Pidl {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.cast())) }
    }
}

pub(super) async fn show_files(paths: Vec<PathBuf>) -> Result<Interaction> {
    sta(
        || Ok(()),
        move |()| {
            let mut folders = BTreeMap::<PathBuf, Vec<Pidl>>::new();
            for path in paths {
                let parent = path.parent().context("file parent")?.to_owned();
                folders.entry(parent).or_default().push(Pidl::parse(&path)?);
            }
            for (parent, items) in folders {
                // A daemon started with SW_HIDE can otherwise create a hidden
                // Explorer window. Give folder opening an explicit show state.
                execute(&parent)?;
                let folder = Pidl::parse(&parent)?;
                let children: Vec<*const ITEMIDLIST> = items
                    .iter()
                    .map(|item| unsafe { ILFindLastID(item.0).cast_const() })
                    .collect();
                unsafe { SHOpenFolderAndSelectItems(folder.0, Some(&children), 0)? };
            }
            Ok(())
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{DesktopActions, PcActions};

    #[tokio::test]
    async fn cancelled_request_before_issuance_does_not_launch() -> Result<()> {
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let (launched, completed) = tokio::sync::oneshot::channel();
        let mut request = Box::pin(sta(
            move || {
                let _ = started.send(());
                gate.recv_timeout(std::time::Duration::from_secs(5))?;
                Ok(())
            },
            move |()| {
                let _ = launched.send(());
                Ok(())
            },
        ));
        tokio::select! {
            result = &mut request => panic!("returned before issuance: {}", result.is_ok()),
            result = ready => result?,
        }
        drop(request);
        release.send(())?;
        // The worker drops the effect (and its sender) without running it.
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(5), completed)
                .await?
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn received_path_keeps_identity_and_allows_edits() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("received.txt");
        std::fs::write(&path, b"original")?;
        let file = ReceivedFile::open(&path)?;
        assert_eq!(checked_path(&file)?, path);
        std::fs::write(&path, b"edited")?;
        assert_eq!(checked_path(&file)?, path);
        Ok(())
    }

    #[tokio::test]
    async fn received_path_rejects_replacement_deletion_directory_and_reparse() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("received.txt");
        std::fs::write(&path, b"original")?;
        let file = ReceivedFile::open(&path)?;
        std::fs::rename(&path, dir.path().join("retained.txt"))?;
        for kind in ["deleted", "replaced", "directory", "reparse"] {
            let mut file = file.clone();
            let mut existing = false;
            match kind {
                "replaced" => std::fs::write(&path, b"replacement")?,
                "directory" => std::fs::create_dir(&path)?,
                "reparse" => {
                    if let Some(root) = std::env::var_os("PNEU_TEST_SYMLINK_FIXTURE") {
                        let root = PathBuf::from(root);
                        file = ReceivedFile::open(&root.join("real/a/b/file"))?;
                        file.path = root.join("link/a/b/file");
                        existing = true;
                    } else if let Err(error) = std::os::windows::fs::symlink_file(
                        file.path().with_file_name("retained.txt"),
                        &path,
                    ) {
                        if error.raw_os_error() == Some(1314) {
                            eprintln!(
                                "symlink assertion skipped: privilege unavailable; set PNEU_TEST_SYMLINK_FIXTURE"
                            );
                            continue;
                        }
                        return Err(error.into());
                    }
                }
                _ => {}
            }
            assert!(
                checked_path(&file).unwrap_err().is::<FileUnavailable>(),
                "{kind}"
            );
            let actions = DesktopActions::new(PathBuf::new());
            assert!(
                actions
                    .open_received(&file)
                    .await
                    .unwrap_err()
                    .is::<FileUnavailable>(),
                "{kind}"
            );
            assert!(
                actions
                    .begin_open_received(&file)
                    .await
                    .err()
                    .context("expected unavailable before issuance")?
                    .is::<FileUnavailable>(),
                "{kind}"
            );
            if kind == "directory" {
                std::fs::remove_dir(&path)?;
            } else if kind != "deleted" && !existing {
                std::fs::remove_file(&path)?;
            }
        }
        assert!(
            DesktopActions::new(PathBuf::new())
                .open(&path)
                .await
                .unwrap_err()
                .is::<FileUnavailable>()
        );
        Ok(())
    }

    #[tokio::test]
    async fn folder_open_rejects_missing_and_regular_file() -> Result<()> {
        let dir = crate::test_support::scratch()?;
        let path = dir.path().join("missing");
        assert!(open_folder(path.clone()).await.is_err());
        assert!(!path.exists());
        std::fs::write(&path, b"file")?;
        assert!(open_folder(path).await.is_err());
        Ok(())
    }

    #[tokio::test]
    #[ignore = "requires an interactive console and PNEU_TEST_SHOW_FILES JSON paths"]
    async fn native_show_files() -> Result<()> {
        let paths = serde_json::from_str(&std::env::var("PNEU_TEST_SHOW_FILES")?)?;
        show_files(paths).await?.await
    }
}
