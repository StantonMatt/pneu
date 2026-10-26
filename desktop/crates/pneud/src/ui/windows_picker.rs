//! Native pickers run on Slint's STA thread and are owned by its active HWND.
use anyhow::Result;
use std::{
    os::windows::ffi::{OsStrExt, OsStringExt},
    path::{Path, PathBuf},
};
use windows::{
    Win32::{
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
            CoTaskMemFree, CoUninitialize,
        },
        UI::{
            Input::KeyboardAndMouse::GetActiveWindow,
            Shell::{
                FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST,
                FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem,
                SHCreateItemFromParsingName, SIGDN_FILESYSPATH,
            },
        },
    },
    core::{HRESULT, HSTRING, PCWSTR},
};

pub(super) async fn choose(title: &str, folder: Option<&Path>) -> Result<Vec<PathBuf>> {
    let title = title.to_owned();
    let folder = folder.map(Path::to_owned);
    let (send, receive) = tokio::sync::oneshot::channel();
    slint::invoke_from_event_loop(move || {
        let _ = send.send(choose_on_ui(&title, folder.as_deref()));
    })?;
    receive.await?
}
fn choose_on_ui(title: &str, folder: Option<&Path>) -> Result<Vec<PathBuf>> {
    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() }
        }
    }
    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED).ok()?;
        let _apartment = Apartment;
        let dialog: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
        let options = dialog.GetOptions()? | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
        dialog.SetOptions(
            options
                | if folder.is_some() {
                    FOS_PICKFOLDERS
                } else {
                    FOS_ALLOWMULTISELECT | FOS_FILEMUSTEXIST
                },
        )?;
        dialog.SetTitle(&HSTRING::from(title))?;
        if let Some(folder) = folder {
            let path: Vec<u16> = folder.as_os_str().encode_wide().chain(Some(0)).collect();
            if let Ok(item) =
                SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(path.as_ptr()), None)
            {
                dialog.SetFolder(&item)?;
            }
        }
        match dialog.Show(Some(GetActiveWindow())) {
            Err(error) if error.code() == HRESULT(0x800704c7u32 as i32) => return Ok(Vec::new()),
            result => result?,
        }
        let items = dialog.GetResults()?;
        let mut paths = Vec::new();
        for index in 0..items.GetCount()? {
            let name = items.GetItemAt(index)?.GetDisplayName(SIGDN_FILESYSPATH)?;
            let path = std::ffi::OsString::from_wide(name.as_wide());
            CoTaskMemFree(Some(name.0.cast()));
            paths.push(PathBuf::from(path));
        }
        Ok(paths)
    }
}
