// Included in core::tests to reuse its isolated fixture, never the OS session.
#[derive(Default)]
struct SmsActions {
    notices:Mutex<Vec<crate::ui::notifications::Notice>>,
    clipboard:Mutex<Vec<(String,bool)>>,
    locked:AtomicBool,
    fail:AtomicBool,
    drains:AtomicU64,
    block_prepare:AtomicBool,
    prepare_entered:tokio::sync::Notify,
    prepare_release:tokio::sync::Notify,
}
#[async_trait::async_trait]
impl PcActions for SmsActions {
    async fn drain_clipboard(&self) {self.drains.fetch_add(1,Ordering::SeqCst);}
    async fn screen_locked(&self)->bool {self.locked.load(Ordering::SeqCst)}
    async fn notify_sms(&self,n:crate::ui::notifications::Notice,_:u32)->Result<u32> {
        let mut notices=self.notices.lock().unwrap();notices.push(n);Ok(notices.len() as u32)
    }
    async fn clipboard_text(&self,_:&str)->Result<()> {Ok(())}
    async fn prepare_clipboard(&self,input:crate::actions::ClipboardInput,_:u128)->Result<crate::actions::PreparedClipboard> {
        if self.block_prepare.load(Ordering::SeqCst) {
            self.prepare_entered.notify_one();
            self.prepare_release.notified().await;
        }
        crate::actions::PreparedClipboard::immediate(input)
    }
    async fn install_clipboard(&self,prepared:&mut crate::actions::PreparedClipboard)->Result<()> {
        ensure!(!self.fail.load(Ordering::SeqCst),"injected write failure");
        match prepared.input() {
            crate::actions::ClipboardInput::SecretText(t)=>self.clipboard.lock().unwrap().push((t.0.clone(),true)),
            crate::actions::ClipboardInput::Text(t)=>self.clipboard.lock().unwrap().push((t.clone(),false)),
            _=>bail!("unexpected test input"),
        }
        Ok(())
    }
    async fn clipboard_image(&self,_:&std::path::Path,_:&str)->Result<()> {Ok(())}
    async fn notify(&self,_:&std::path::Path,_:&str)->Result<()> {Ok(())}
    async fn open(&self,_:&std::path::Path)->Result<()> {Ok(())}
}
fn sms_message(text:&str)->Sms {Sms {
    transfer_id:Id::new(crate::random_hex(16)).unwrap(),sender:"BancoEstado".into(),text:text.into(),received_at:crate::now_ms(),
}}
#[tokio::test]
async fn sms_receive_masks_copies_secret_deduplicates_and_never_persists_content()->Result<()> {
    let actions=Arc::new(SmsActions::default());
    let (dir,core,device)=fixture(actions.clone()).await?;
    let mut config=core.config();config.notification_preview=false;core.config.send_replace(config);
    let (tx,mut rx)=mpsc::channel(10);
    let message=sms_message("Tu clave dinámica es 482913. No la compartas.");
    core.receive_test(&core.store,&device,Message::Sms(message.clone()),&mut None,&tx).await?;
    assert!(matches!(rx.recv().await,Some(Message::Ack(Ack{deduplicated:false,..}))));
    assert_eq!(*actions.clipboard.lock().unwrap(),vec![("482913".into(),true)]);
    let n=actions.notices.lock().unwrap()[0].clone();
    assert_eq!(n.summary,"Code copied from BancoEstado");assert!(n.body.contains("••••••"));assert!(!n.body.contains("482913"));
    assert_eq!(n.actions,vec![("sms-code","Copy Code"),("sms-text","Copy Text")]);
    assert!(!n.transient);assert_eq!((n.urgency,n.expires),(1,-1));
    core.receive_test(&core.store,&device,Message::Sms(message),&mut None,&tx).await?;
    assert!(matches!(rx.recv().await,Some(Message::Ack(Ack{deduplicated:true,..}))));
    assert_eq!(actions.notices.lock().unwrap().len(),1);
    let bytes=std::fs::read(dir.path().join("data/state.sqlite3"))?;
    assert!(!bytes.windows(6).any(|w|w==b"482913"));
    // Explicit clipboard sharing fails closed on the retained secret payload.
    core.clipboard_test_idle().await;
    assert!(core.sms.lock().unwrap().messages.len()==1);
    let _session=admit_ui_test_phone(&core,&device,1)?;
    assert!(core.share_clipboard(crate::outbox::Targets::Devices(vec![device.clone()])).await.is_err());
    core.shutdown_actions().await
}
#[tokio::test]
async fn sms_locked_disabled_stale_and_ambiguous_do_not_copy()->Result<()> {
    let actions=Arc::new(SmsActions::default());actions.locked.store(true,Ordering::SeqCst);
    let (_dir,core,device)=fixture(actions.clone()).await?;
    let (tx,mut rx)=mpsc::channel(10);
    core.receive_test(&core.store,&device,Message::Sms(sms_message("OTP 482913")),&mut None,&tx).await?;rx.recv().await;
    assert!(actions.clipboard.lock().unwrap().is_empty());
    assert_eq!(actions.notices.lock().unwrap()[0].summary,"BancoEstado");
    assert!(actions.notices.lock().unwrap()[0].body.contains("••••••"));
    actions.locked.store(false,Ordering::SeqCst);
    let mut config=core.config();config.copy_sms_codes=false;core.config.send_replace(config);
    core.receive_test(&core.store,&device,Message::Sms(sms_message("OTP 7731-22")),&mut None,&tx).await?;rx.recv().await;
    core.receive_test(&core.store,&device,Message::Sms(sms_message("Tu código es 4821 o 9921")),&mut None,&tx).await?;rx.recv().await;
    assert!(actions.notices.lock().unwrap()[2].body.contains("4821 o 9921"));
    assert_eq!(actions.notices.lock().unwrap()[2].actions,vec![("sms-text","Copy Text")]);
    let mut stale=sms_message("OTP 333444");stale.received_at=crate::now_ms()-120_001;
    core.receive_test(&core.store,&device,Message::Sms(stale),&mut None,&tx).await?;rx.recv().await;
    assert_eq!(actions.notices.lock().unwrap().len(),3);assert!(actions.clipboard.lock().unwrap().is_empty());
    core.shutdown_actions().await
}
#[tokio::test]
async fn sms_newer_clipboard_wins_and_manual_copy_retries_with_secret_handling()->Result<()> {
    let actions=Arc::new(SmsActions::default());let (_dir,core,device)=fixture(actions.clone()).await?;
    let mut message=sms_message("OTP 482913");message.received_at-=1000;
    let (_,token)=core.observe_watched_clipboard(false);
    core.clipboard_test_read(token,Some(crate::ui::clipboard::Content::Text("newer human copy".into()))).await;
    let (tx,mut rx)=mpsc::channel(10);
    core.receive_test(&core.store,&device,Message::Sms(message.clone()),&mut None,&tx).await?;rx.recv().await;
    assert!(actions.clipboard.lock().unwrap().is_empty());
    assert_eq!(actions.notices.lock().unwrap()[0].summary,"BancoEstado");
    let target=crate::sms::Target{device,id:message.transfer_id};
    core.copy_sms(target.clone(),true).await;
    core.copy_sms(target,false).await;
    assert_eq!(*actions.clipboard.lock().unwrap(),vec![("482913".into(),true),("OTP 482913".into(),true)]);
    core.shutdown_actions().await
}
#[tokio::test]
async fn sms_copy_failure_is_masked_and_pair_off_rejects_without_notice()->Result<()> {
    let actions=Arc::new(SmsActions::default());actions.fail.store(true,Ordering::SeqCst);
    let (_dir,core,device)=fixture(actions.clone()).await?;let (tx,mut rx)=mpsc::channel(10);
    core.receive_test(&core.store,&device,Message::Sms(sms_message("OTP 482913")),&mut None,&tx).await?;rx.recv().await;
    assert_eq!(actions.notices.lock().unwrap()[0].summary,"Couldn't copy code from BancoEstado");
    core.store.pair_settings.lock().unwrap().insert(device.clone(),vec![PeerSetting {
        key:format!("{device}/{}/sms",core.identity.device_id),enabled:false,hlc:1,writer_device:device.clone(),
    }]);
    core.receive_test(&core.store,&device,Message::Sms(sms_message("OTP 123456")),&mut None,&tx).await?;
    assert!(matches!(rx.recv().await,Some(Message::Nack(Nack{code:NackCode::UnsupportedKind,..}))));
    assert_eq!(actions.notices.lock().unwrap().len(),1);
    core.shutdown_actions().await
}

#[tokio::test]
async fn sms_mesh_copy_uses_sms_policy_even_when_clipboard_receive_is_off()->Result<()> {
    let actions=Arc::new(SmsActions::default());let (_dir,core,device)=fixture(actions.clone()).await?;
    let hello=Hello{ sms_activated: None,version:1,device_id:device.clone(),name:"SMS phone".into(),capabilities:vec!["mesh".into(),"sms".into()],role:Some(DeviceRole::Phone),addresses:None,paused:None};
    core.store.remember_peer_hello(hello,Arc::new(AtomicU64::new(1)),1,false).await?;
    core.store.settings_fields(core.identity.device_id.clone(),device.clone(),true).await?;
    core.store.settings_edit(core.identity.device_id.clone(),device.clone(),true,format!("{device}/{}/clipboard",core.identity.device_id),false).await?;
    let mut store=core.store.clone();store.mesh=true;
    let (tx,mut rx)=mpsc::channel(10);
    core.receive_test(&store,&device,Message::Sms(sms_message("OTP 482913")),&mut None,&tx).await?;rx.recv().await;
    assert_eq!(*actions.clipboard.lock().unwrap(),vec![("482913".into(),true)]);
    core.shutdown_actions().await
}

#[tokio::test]
async fn sms_notification_actions_are_revoked_when_the_phone_is_unpaired()->Result<()> {
    let actions=Arc::new(SmsActions::default());let (_dir,core,device)=fixture(actions.clone()).await?;
    let _session=admit_ui_test_phone(&core,&device,1)?;
    let mut config=core.config();config.copy_sms_codes=false;core.config.send_replace(config);
    let message=sms_message("OTP 482913");let target=crate::sms::Target{device:device.clone(),id:message.transfer_id.clone()};
    let (tx,mut rx)=mpsc::channel(10);
    core.receive_test(&core.store,&device,Message::Sms(message),&mut None,&tx).await?;rx.recv().await;
    core.unpair_device(device).await?;assert!(core.sms.lock().unwrap().messages.is_empty());
    core.copy_sms(target,true).await;assert!(actions.clipboard.lock().unwrap().is_empty());
    core.shutdown_actions().await
}

#[tokio::test]
async fn sms_secret_helper_lives_for_sixty_seconds_and_expires_without_persistence()->Result<()> {
    let actions=Arc::new(SmsActions::default());let (_dir,core,device)=fixture(actions.clone()).await?;
    let (tx,mut rx)=mpsc::channel(10);
    core.receive_test(&core.store,&device,Message::Sms(sms_message("OTP 482913")),&mut None,&tx).await?;rx.recv().await;
    assert_eq!(actions.drains.load(Ordering::SeqCst),0);
    tokio::time::pause();tokio::time::advance(Duration::from_secs(59)).await;
    for _ in 0..20 {tokio::task::yield_now().await;}
    assert_eq!(actions.drains.load(Ordering::SeqCst),0,"must not clear before 60 seconds");
    tokio::time::advance(Duration::from_secs(2)).await;
    for _ in 0..100 {tokio::task::yield_now().await;}
    assert_eq!(actions.drains.load(Ordering::SeqCst),1,"the secret helper is stopped and reaped at the deadline");
    tokio::time::resume();core.shutdown_actions().await
}

#[tokio::test]
async fn sms_settings_stay_hidden_until_the_phone_has_opted_in_once()->Result<()> {
    let (_dir,core,device)=fixture(Arc::new(SmsActions::default())).await?;
    let mut hello=Hello{sms_activated:None,version:1,device_id:device.clone(),name:"SMS phone".into(),capabilities:vec!["mesh".into(),"sms".into()],role:Some(DeviceRole::Phone),addresses:None,paused:None};
    let current=Arc::new(AtomicU64::new(1));
    core.store.remember_peer_hello(hello.clone(),current.clone(),1,false).await?;
    core.store.settings_fields(core.identity.device_id.clone(),device.clone(),true).await?;
    assert!(!core.ui_snapshot().sms_available,"capability alone must not show the section");
    hello.sms_activated=Some(true);core.store.remember_peer_hello(hello.clone(),current.clone(),1,false).await?;
    assert!(core.ui_snapshot().sms_available);
    hello.sms_activated=Some(false);core.store.remember_peer_hello(hello,current,1,false).await?;
    assert!(core.ui_snapshot().sms_available,"keep the setting available after first activation");
    core.shutdown_actions().await
}

#[tokio::test]
async fn sms_burst_sender_retention_is_bounded() -> Result<()> {
    let actions = Arc::new(SmsActions::default());
    let (_dir, core, device) = fixture(actions.clone()).await?;
    let (tx, mut rx) = mpsc::channel(1);
    for i in 0..300 {
        let mut message = sms_message("Ordinary message");
        message.sender = format!("sender {i}");
        core.receive_test(&core.store, &device, Message::Sms(message), &mut None, &tx).await?;
        assert!(matches!(rx.recv().await, Some(Message::Ack(_))));
    }
    {
        let state = core.sms.lock().unwrap();
        let burst = &state.bursts[&device];
        assert_eq!(burst.count, 300);
        assert_eq!(burst.senders.len(), 3);
        assert_eq!(state.messages.len(), 256);
    }
    assert!(actions.notices.lock().unwrap().last().unwrap().body.ends_with("and others"));
    core.shutdown_actions().await
}

#[tokio::test]
async fn sms_stale_receive_cannot_repopulate_targets_after_unpair() -> Result<()> {
    let actions = Arc::new(SmsActions::default());
    let (_dir, core, device) = fixture(actions.clone()).await?;
    let _session = admit_ui_test_phone(&core, &device, 1)?;
    let scope = core.action_scope.lock().unwrap()[&device].clone();
    let origin = ActionOrigin::captured(&core.store, &device);
    assert!(scope.allows(&origin)); // A receive passed its initial admission check.
    let message = sms_message("Old text");
    let target = crate::sms::Target { device: device.clone(), id: message.transfer_id.clone() };
    core.unpair_device(device.clone()).await?;
    // Its delayed insertion races after the purge, before a new pairing scope.
    assert!(core.remember_sms(&core.store, &scope, &origin, target.clone(), message).is_none());
    core.action_scope.lock().unwrap().insert(device, ActionScope::new());
    core.copy_sms(target, false).await;
    assert!(core.sms.lock().unwrap().messages.is_empty());
    assert!(actions.clipboard.lock().unwrap().is_empty());
    core.shutdown_actions().await
}

#[tokio::test]
async fn sms_ordinary_copy_rechecks_sms_permission_after_preparation() -> Result<()> {
    let actions = Arc::new(SmsActions::default());
    let (_dir, core, device) = fixture(actions.clone()).await?;
    let message = sms_message("An ordinary message");
    let target = crate::sms::Target { device: device.clone(), id: message.transfer_id.clone() };
    let (tx, mut rx) = mpsc::channel(10);
    core.receive_test(&core.store, &device, Message::Sms(message), &mut None, &tx).await?;
    rx.recv().await;
    actions.block_prepare.store(true, Ordering::SeqCst);
    let copying = tokio::spawn({let core=core.clone(); async move {core.copy_sms(target, false).await}});
    tokio::time::timeout(Duration::from_secs(2), actions.prepare_entered.notified()).await?;
    core.store.pair_settings.lock().unwrap().insert(device.clone(), vec![PeerSetting {
        key:format!("{device}/{}/sms",core.identity.device_id), enabled:false, hlc:1, writer_device:device,
    }]);
    core.store.settings_changed.send_modify(|r| *r=r.wrapping_add(1));
    actions.prepare_release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), copying).await??;
    assert!(actions.clipboard.lock().unwrap().is_empty());
    core.shutdown_actions().await
}

#[tokio::test]
async fn sms_installed_secret_survives_reconnect_until_expiry_or_explicit_revocation() -> Result<()> {
    for ending in ["expiry", "policy", "unpair"] {
        let actions = Arc::new(SmsActions::default());
        let (_dir, core, device) = fixture(actions.clone()).await?;
        let _session = admit_ui_test_phone(&core, &device, 1)?;
        let scope = core.action_scope.lock().unwrap()[&device].clone();
        let mut store = core.store.clone();
        store.action_connection_id = Some(1);
        let (tx, mut rx) = mpsc::channel(10);
        core.receive_test(&store, &device, Message::Sms(sms_message("OTP 482913")), &mut None, &tx).await?;
        rx.recv().await;
        core.clipboard_test_idle().await;
        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(20)).await;
        scope.connection_generation.store(2, Ordering::SeqCst);
        scope.generation_changed.send_replace(2);
        for _ in 0..100 { tokio::task::yield_now().await; }
        assert_eq!(actions.drains.load(Ordering::SeqCst), 0, "reconnection must not clear an installed secret");
        if ending == "expiry" {
            tokio::time::advance(Duration::from_secs(39)).await;
            for _ in 0..100 { tokio::task::yield_now().await; }
            assert_eq!(actions.drains.load(Ordering::SeqCst), 0, "must preserve the original 60-second deadline");
            tokio::time::advance(Duration::from_secs(2)).await;
        }
        tokio::time::resume();
        if ending == "policy" {
            core.store.pair_settings.lock().unwrap().insert(device.clone(), vec![PeerSetting {
                key:format!("{device}/{}/sms",core.identity.device_id), enabled:false, hlc:1, writer_device:device,
            }]);
            core.store.settings_changed.send_modify(|r| *r=r.wrapping_add(1));
        } else if ending == "unpair" {
            core.unpair_device(device).await?;
        }
        for _ in 0..100 { tokio::task::yield_now().await; }
        assert_eq!(actions.drains.load(Ordering::SeqCst), 1, "secret must clear on {ending}");
        core.shutdown_actions().await?;
    }
    Ok(())
}
