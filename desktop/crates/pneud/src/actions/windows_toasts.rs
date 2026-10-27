//! The daemon's STA owns WinRT notifications and its local COM activator.
use crate::ui::{notice_lifecycle::Lifecycle, portal::Interaction};
use anyhow::{Context, Result};
use std::{
    ffi::c_void,
    path::PathBuf,
    sync::{Arc, mpsc},
    time::Duration,
};
use tokio::sync::oneshot;
use windows::{
    Data::Xml::Dom::XmlDocument,
    Foundation::{DateTime, IReference, PropertyValue},
    UI::Notifications::{ToastNotification, ToastNotificationManager, ToastNotifier},
    Win32::{
        Storage::Packaging::Appx::GetCurrentPackageFullName,
        System::{
            Com::*,
            Registry::*,
            WinRT::{RO_INIT_SINGLETHREADED, RoInitialize, RoUninitialize},
        },
        UI::{
            Notifications::*, Shell::SetCurrentProcessExplicitAppUserModelID,
            WindowsAndMessaging::*,
        },
    },
    core::{BOOL, Error, GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR, Ref, implement},
};

pub(super) const AUMID: &str = "StantonMatt.Pneu";
// Stable Pneu toast activator. M6 must declare this same CLSID in both the
// toastNotificationActivation and comServer MSIX manifest extensions.
pub(super) const CLSID_TEXT: &str = "{9f3b5a91-4c6f-4ad1-a7d9-51e647ee32b8}";
const CLSID: GUID = GUID::from_u128(0x9f3b5a91_4c6f_4ad1_a7d9_51e647ee32b8);
pub(crate) struct Activation {
    pub arguments: String,
    pub inputs: Vec<(String, String)>,
}
#[implement(INotificationActivationCallback)]
struct Callback {
    send: tokio::sync::mpsc::Sender<Activation>,
}
impl INotificationActivationCallback_Impl for Callback_Impl {
    fn Activate(
        &self,
        _aumid: &PCWSTR,
        args: &PCWSTR,
        data: *const NOTIFICATION_USER_INPUT_DATA,
        count: u32,
    ) -> windows::core::Result<()> {
        let invalid = || Error::from_hresult(HRESULT(0x80070057u32 as i32));
        if count > 16 || (count != 0 && data.is_null()) {
            return Err(invalid());
        }
        unsafe fn bounded(value: PCWSTR, limit: usize) -> windows::core::Result<String> {
            if value.is_null() {
                return Ok(String::new());
            }
            for n in 0..=limit {
                if unsafe { *value.0.add(n) } == 0 {
                    return String::from_utf16(unsafe { std::slice::from_raw_parts(value.0, n) })
                        .map_err(|_| Error::from_hresult(HRESULT(0x80070057u32 as i32)));
                }
            }
            Err(Error::from_hresult(HRESULT(0x80070057u32 as i32)))
        }
        let arguments = unsafe { bounded(*args, 256)? };
        let mut inputs = Vec::new();
        for n in 0..count as usize {
            let input = unsafe { &*data.add(n) };
            inputs.push((unsafe { bounded(input.Key, 64)? }, unsafe {
                bounded(input.Value, 1024)?
            }));
        }
        self.send
            .try_send(Activation { arguments, inputs })
            .map_err(|_| Error::from_hresult(HRESULT(0x800700aau32 as i32)))
    }
}
#[implement(IClassFactory)]
struct Factory {
    send: tokio::sync::mpsc::Sender<Activation>,
}
impl IClassFactory_Impl for Factory_Impl {
    fn CreateInstance(
        &self,
        outer: Ref<IUnknown>,
        riid: *const GUID,
        out: *mut *mut c_void,
    ) -> windows::core::Result<()> {
        if outer.is_some() {
            return Err(Error::from_hresult(HRESULT(0x80040110u32 as i32)));
        }
        let callback: INotificationActivationCallback = Callback {
            send: self.send.clone(),
        }
        .into();
        unsafe { callback.query(riid, out).ok() }
    }
    fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
        Ok(())
    }
}

enum Command {
    Show {
        xml: String,
        tag: String,
        group: String,
        life: Arc<Lifecycle>,
        reply: oneshot::Sender<Result<()>>,
    },
    Remove {
        tag: String,
        group: String,
    },
    Stop,
}
pub(super) struct Host {
    send: mpsc::Sender<Command>,
    thread: Option<std::thread::JoinHandle<()>>,
}
#[derive(Clone)]
pub(super) struct Sender(mpsc::Sender<Command>);
impl Sender {
    pub fn show(
        &self,
        xml: String,
        tag: String,
        group: String,
        life: Arc<Lifecycle>,
    ) -> Result<Interaction> {
        let (send, receive) = oneshot::channel();
        self.0
            .send(Command::Show {
                xml,
                tag,
                group,
                life,
                reply: send,
            })
            .context("toast thread stopped")?;
        Ok(Box::pin(async move {
            receive.await.context("toast submission ended")?
        }))
    }
    pub fn remove(&self, id: u32, group: &str) {
        let _ = self.0.send(Command::Remove {
            tag: id.to_string(),
            group: group.into(),
        });
    }
}
impl Host {
    pub async fn start(cache: PathBuf) -> Result<(Self, tokio::sync::mpsc::Receiver<Activation>)> {
        let (send, receive) = mpsc::channel();
        let (activation, activations) = tokio::sync::mpsc::channel(32);
        let (ready, started) = oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("pneu-toasts".into())
            .spawn(move || {
                let mut ready = Some(ready);
                let result = (|| -> Result<()> {
                    unsafe {
                        RoInitialize(RO_INIT_SINGLETHREADED)?;
                    }
                    struct Apartment;
                    impl Drop for Apartment {
                        fn drop(&mut self) {
                            unsafe { RoUninitialize() }
                        }
                    }
                    let _apartment = Apartment;
                    let packaged = package_identity()?;
                    if !packaged {
                        register_identity(cache)?;
                    }
                    let factory: IClassFactory = Factory { send: activation }.into();
                    let cookie = unsafe {
                        CoRegisterClassObject(
                            &CLSID,
                            &factory,
                            CLSCTX_LOCAL_SERVER,
                            REGCLS_MULTIPLEUSE,
                        )?
                    };
                    struct Registration(u32);
                    impl Drop for Registration {
                        fn drop(&mut self) {
                            let _ = unsafe { CoRevokeClassObject(self.0) };
                        }
                    }
                    let _registration = Registration(cookie);
                    let notifier = if packaged {
                        ToastNotificationManager::CreateToastNotifier()?
                    } else {
                        ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?
                    };
                    let _ = ready.take().unwrap().send(Ok(()));
                    eprintln!("pneud: toast activator ready packaged={packaged}");
                    loop {
                        let mut msg = MSG::default();
                        unsafe {
                            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                                if msg.message == WM_QUIT {
                                    return Ok(());
                                }
                                let _ = TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                        }
                        match receive.recv_timeout(Duration::from_millis(20)) {
                            Ok(Command::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                            Err(mpsc::RecvTimeoutError::Timeout) => {}
                            Ok(Command::Remove { tag, group }) => {
                                let _ = remove(&tag, &group, packaged);
                            }
                            Ok(Command::Show {
                                xml,
                                tag,
                                group,
                                life,
                                reply,
                            }) => {
                                let result =
                                    life.run(|| show(&notifier, &xml, &tag, &group, &life));
                                let _ = reply.send(result);
                            }
                        }
                    }
                    Ok(())
                })();
                if let Some(ready) = ready {
                    let _ = ready.send(result);
                } else if let Err(error) = result {
                    eprintln!("pneud: toast thread stopped: {error:#}");
                }
            })?;
        match started.await.context("toast thread startup ended")? {
            Ok(()) => Ok((
                Self {
                    send,
                    thread: Some(thread),
                },
                activations,
            )),
            Err(error) => {
                let _ = thread.join();
                Err(error)
            }
        }
    }
    pub fn sender(&self) -> Sender {
        Sender(self.send.clone())
    }
    pub async fn shutdown(mut self) {
        let _ = self.send.send(Command::Stop);
        if let Some(thread) = self.thread.take() {
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.send.send(Command::Stop);
    }
}
fn show(
    notifier: &ToastNotifier,
    xml: &str,
    tag: &str,
    group: &str,
    life: &Lifecycle,
) -> Result<()> {
    let document = XmlDocument::new()?;
    document.LoadXml(&HSTRING::from(xml))?;
    let toast = ToastNotification::CreateToastNotification(&document)?;
    toast.SetTag(&HSTRING::from(tag))?;
    toast.SetGroup(&HSTRING::from(group))?;
    if let Some(deadline) = life.deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let unix = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)? + remaining;
        let expiration = DateTime {
            UniversalTime: (unix.as_nanos() / 100) as i64 + 116444736000000000,
        };
        let expiration: IReference<DateTime> = PropertyValue::CreateDateTime(expiration)?.cast()?;
        toast.SetExpirationTime(&expiration)?;
    }
    // Setting() can return ELEMENT_NOT_FOUND on the first unpackaged toast.
    notifier.Show(&toast)?;
    Ok(())
}
fn remove(tag: &str, group: &str, packaged: bool) -> Result<()> {
    let history = ToastNotificationManager::History()?;
    if packaged {
        history.RemoveGroupedTag(&HSTRING::from(tag), &HSTRING::from(group))?;
    } else {
        history.RemoveGroupedTagWithId(
            &HSTRING::from(tag),
            &HSTRING::from(group),
            &HSTRING::from(AUMID),
        )?;
    }
    Ok(())
}
fn package_identity() -> Result<bool> {
    let mut length = 0;
    let result = unsafe { GetCurrentPackageFullName(&mut length, None) };
    match result.0 {
        122 => Ok(true),
        15700 => Ok(false),
        _ => {
            result.ok()?;
            Ok(true)
        }
    }
}
fn wide(value: &str) -> Result<Vec<u16>> {
    let mut value: Vec<u16> = value.encode_utf16().collect();
    anyhow::ensure!(
        !value.contains(&0),
        "registration contains a null character"
    );
    value.push(0);
    Ok(value)
}
fn register_identity(cache: PathBuf) -> Result<()> {
    let icon = cache.join("notification-icon.png");
    let pixels =
        crate::ui::tray_model::render_svg(include_bytes!("../../../../assets/pneu.svg"), 64)?;
    image::save_buffer_with_format(
        &icon,
        &pixels,
        64,
        64,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )?;
    let app = format!(r"Software\Classes\AppUserModelId\{AUMID}");
    set_value(&app, "DisplayName", "Pneu")?;
    set_value(
        &app,
        "IconUri",
        icon.to_str().context("toast icon path is not Unicode")?,
    )?;
    set_value(&app, "CustomActivator", CLSID_TEXT)?;
    let exe = std::env::current_exe()?;
    let exe = exe.to_str().context("toast server path is not Unicode")?;
    anyhow::ensure!(!exe.contains('"'), "invalid toast server path");
    set_value(
        &format!(r"Software\Classes\CLSID\{CLSID_TEXT}\LocalServer32"),
        "",
        &format!("\"{exe}\" -Embedding"),
    )?;
    let id = wide(AUMID)?;
    unsafe {
        SetCurrentProcessExplicitAppUserModelID(PCWSTR(id.as_ptr()))?;
    }
    Ok(())
}
fn set_value(key: &str, name: &str, value: &str) -> Result<()> {
    let key = wide(key)?;
    let name = wide(name)?;
    let value = wide(value)?;
    let mut handle = HKEY::default();
    unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(key.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut handle,
            None,
        )
        .ok()?;
        let result = RegSetKeyValueW(
            handle,
            None,
            PCWSTR(name.as_ptr()),
            REG_SZ.0,
            Some(value.as_ptr().cast()),
            (value.len() * 2) as u32,
        )
        .ok();
        let _ = RegCloseKey(handle);
        result?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{DesktopActions, Target};
    use super::*;
    use crate::ui::notifications::Notice;

    #[tokio::test(start_paused = true)]
    async fn withdrawal_precedes_reused_tag_for_activation_cancellation_and_expiry() -> Result<()> {
        for mode in ["activation", "cancellation", "expiry"] {
            let actions = DesktopActions::new(PathBuf::new());
            let (send, receive) = mpsc::channel();
            assert!(actions.sender.set(Sender(send)).is_ok());
            let notice = if mode == "expiry" {
                Notice::text("first")
            } else {
                Notice::new("first")
            };
            let (id, wait) = actions
                .issue_notice_target(notice, Target::default(), 0)
                .await?;
            let Command::Show { reply, .. } = receive.try_recv()? else {
                panic!("missing first show")
            };
            reply.send(Ok(())).unwrap();
            let record = actions.targets.lock().unwrap().records[&id].clone();
            match mode {
                "activation" => {
                    actions.take_notification(&record.argument("window"))?;
                }
                "cancellation" => {
                    drop(wait);
                }
                _ => {
                    wait.await?;
                    // Register the timer before advancing the paused clock.
                    tokio::task::yield_now().await;
                    tokio::time::advance(Duration::from_secs(4)).await;
                    tokio::task::yield_now().await;
                }
            }
            // Withdrawal must already be queued when the target becomes absent.
            {
                let targets = actions.targets.lock().unwrap();
                assert!(!targets.records.contains_key(&id));
                assert!(
                    matches!(receive.try_recv()?, Command::Remove { tag, .. } if tag == id.to_string())
                );
            }
            let (_, replacement) = actions
                .issue_notice_target(Notice::new("next"), Target::default(), id)
                .await?;
            let Command::Show { tag, reply, .. } = receive.try_recv()? else {
                panic!("missing replacement show")
            };
            assert_eq!(tag, id.to_string());
            reply.send(Ok(())).unwrap();
            replacement.await?;
            assert!(receive.try_recv().is_err());
            assert!(actions.targets.lock().unwrap().records[&id].life.active());
        }
        Ok(())
    }
}
