//! Pair once, join your devices: what the window shows from the daemon's
//! setup status. The screens never say "setup". They say "your devices" and
//! "linked", and "paired" stays the word for one direct pairing.
use super::window::{Peer, display_peer_text};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The daemon's outcome when two devices each have their own linked devices.
pub(crate) const SEPARATE: &str = "Direct link kept; separate setups not linked";
/// Allow was answered; the daemon links once that device's HELLO is refreshed.
const ALLOWING: &str = "allow_pending_refresh";
/// Active members, this device included (PROTOCOL.md, MEMBERS).
pub(crate) const LIMIT: usize = 17;
pub(crate) const OWN: &str = "It already has its own linked devices.";

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub(crate) struct Setup {
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub withdrawn: bool,
    #[serde(default)]
    pub roster: Vec<Member>,
    #[serde(default)]
    pub links: Vec<Link>,
    #[serde(default)]
    pub waiting_for: Option<String>,
    #[serde(default)]
    pub binding_generations: HashMap<String, Option<u64>>,
    #[serde(default)]
    pub notices: Vec<Notice>,
    #[serde(default)]
    pub readmissions: Vec<crate::store::members::Readmission>,
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub(crate) struct Conflict {
    pub device_id: String,
    pub fingerprint: String,
    pub diagnostic: String,
}
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub(crate) struct Member {
    pub device_id: String,
    pub fingerprint: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: String,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub(crate) struct Link {
    pub device: String,
    #[serde(default)]
    pub scope: String,
    #[serde(default)]
    pub prompt: bool,
    #[serde(default)]
    pub introducer_name: Option<String>,
    #[serde(default)]
    pub introduced_by: Option<String>,
    #[serde(default)]
    pub outcome: Option<String>,
    #[serde(default)]
    pub negotiated: bool,
    #[serde(default)]
    pub participating: bool,
    /// The scope the other device last advertised. The daemon doesn't report
    /// it yet, so "Link on {device} too" and its leaving the waiting list wait
    /// for it.
    #[serde(default)]
    pub remote_scope: Option<String>,
    #[serde(default)]
    pub hello_seen: bool,
    #[serde(default)]
    pub ready: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
pub(crate) struct Notice {
    pub device: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub introducer_name: String,
    pub binding_generation: u64,
}

/// What Remove from all devices confirms: the member as the page showed it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Removal {
    pub device: String,
    pub setup_generation: u64,
    pub binding_generation: Option<u64>,
    pub fingerprint: String,
}

impl Setup {
    pub(crate) fn parse(status: &Value) -> Self {
        serde_json::from_value(status["setup"].clone()).unwrap_or_default()
    }
    fn active(&self) -> impl Iterator<Item = &Member> {
        self.roster.iter().filter(|m| m.status == "member")
    }
    /// One of your devices: linked here, or added by one of your linked devices.
    pub(crate) fn member(&self, device: &str) -> Option<&Member> {
        self.active().find(|m| m.device_id == device)
    }
    pub(crate) fn link(&self, device: &str) -> Option<&Link> {
        self.links.iter().find(|l| l.device == device)
    }
    pub(crate) fn roster_ids(&self) -> HashSet<String> {
        self.active().map(|m| m.device_id.clone()).collect()
    }
    /// At the limit, a pairing can only be a plain one.
    pub(crate) fn full(&self) -> bool {
        self.active().count() >= LIMIT
    }
    /// Devices that asked to link and haven't been answered here.
    pub(crate) fn requests(&self) -> Vec<&Link> {
        self.links
            .iter()
            .filter(|l| l.prompt && !l.allowing() && !l.separate())
            .collect()
    }
    /// Pairings not decided since the update, to devices with this version.
    pub(crate) fn undecided(&self) -> Vec<&Link> {
        self.links
            .iter()
            .filter(|l| {
                l.scope == "ask" && !l.prompt && l.negotiated && l.hello_seen && !l.separate()
            })
            .collect()
    }
    /// Linked here, and the other device hasn't answered yet.
    pub(crate) fn unanswered(&self) -> Vec<&Link> {
        self.links
            .iter()
            .filter(|l| {
                l.scope == "enabled"
                    && l.hello_seen
                    && !l.participating
                    && !l.separate()
                    && l.remote_scope.as_deref() != Some("disabled")
                    && (l.negotiated || self.waiting_for.as_deref() == Some(&l.device))
            })
            .collect()
    }
    pub(crate) fn removal(&self, device: &str) -> Option<Removal> {
        let member = self.member(device)?;
        Some(Removal {
            device: device.into(),
            setup_generation: self.generation,
            binding_generation: self.binding_generations.get(device).copied().flatten(),
            fingerprint: member.fingerprint.clone(),
        })
    }
    /// Your devices this one learned since `known`, apart from `except`.
    pub(crate) fn learned(&self, known: &HashSet<String>, except: &[&str]) -> Vec<String> {
        self.active()
            .filter(|m| {
                !known.contains(&m.device_id)
                    && !except.contains(&m.device_id.as_str())
                    && self.link(&m.device_id).is_some_and(|l| l.ready)
            })
            .map(|m| display_peer_text(&m.name))
            .collect()
    }
}

impl Link {
    pub(crate) fn separate(&self) -> bool {
        self.outcome.as_deref() == Some(SEPARATE)
    }
    fn allowing(&self) -> bool {
        self.outcome.as_deref() == Some(ALLOWING)
    }
}

/// The row above Unpair or Remove from all devices on a device page.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Row {
    #[default]
    None,
    /// Link with your other devices · Link
    Link,
    /// Asked, no answer yet.
    Waiting,
    /// The other device said Don't allow, or paired with the switch off.
    Theirs,
    /// The other device asked: Don't Allow · Allow.
    Asked,
    /// An older Pneu.
    Update,
    /// It already has its own linked devices.
    Separate,
}

pub(crate) fn row(setup: &Setup, peer: &Peer) -> Row {
    let Some(link) = setup.link(&peer.id) else {
        return Row::None;
    };
    if peer.lost() {
        return Row::None;
    }
    if link.introduced_by.is_some() && !link.hello_seen {
        return Row::None;
    }
    if link.separate() {
        return Row::Separate;
    }
    if link.prompt && !link.allowing() {
        return Row::Asked;
    }
    // Connected without advertising linking: an older Pneu.
    let legacy = peer.connected() && !link.negotiated;
    match link.scope.as_str() {
        // Linked, or linking now that both agreed.
        "enabled" if link.participating && link.hello_seen => Row::None,
        "enabled" if link.remote_scope.as_deref() == Some("disabled") => Row::Theirs,
        "enabled" if legacy => Row::Update,
        "enabled" => Row::Waiting,
        _ if legacy => Row::Update,
        // Allowed here; it links once that device reconnects.
        _ if link.allowing() => Row::None,
        _ => Row::Link,
    }
}

/// Title and second line for a row; `name` is the other device.
pub(crate) fn row_text(row: Row, name: &str) -> (String, String) {
    let link = "Link with your other devices".to_owned();
    match row {
        Row::None => (String::new(), String::new()),
        Row::Link => (link, String::new()),
        Row::Waiting => (link, format!("Allow linking on {name}")),
        Row::Theirs => (link, format!("Link on {name} too")),
        Row::Asked => ("Wants to link your devices".into(), String::new()),
        Row::Update => (link, format!("Update Pneu on {name} to link it")),
        Row::Separate => (
            "Not linked with your other devices".into(),
            OWN.trim_end_matches('.').into(),
        ),
    }
}

/// "A", "A and B", "A and 2 other devices": who asked.
pub(crate) fn askers(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [a, b] => format!("{a} and {b}"),
        [a, rest @ ..] => format!("{a} and {} other devices", rest.len()),
    }
}

/// "on A", "on A and B", "on 3 devices": where to finish linking.
pub(crate) fn waiting_on(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("Allow linking on {one}"),
        [a, b] => format!("Allow linking on {a} and {b}"),
        many => format!("Allow linking on {} devices", many.len()),
    }
}

/// The new device's done screen: two names, then "and {n} more".
pub(crate) fn also_linked(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => format!("Also linked: {one}"),
        [a, b] => format!("Also linked: {a} and {b}"),
        [a, b, rest @ ..] => format!("Also linked: {a}, {b} and\u{a0}{}\u{a0}more", rest.len()),
    }
}

/// The main window's top card.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) enum Top {
    #[default]
    None,
    /// Link your devices · Link · ✕, once after the update.
    Diagnostic {
        title: String,
        key: String,
    },
    Readmit {
        record: Box<crate::store::members::Readmission>,
    },
    Offer {
        device: String,
    },
    /// After Link here: Allow linking on {a} and {b} · ✕.
    Waiting {
        title: String,
    },
    /// {a} wants to link your devices · Don't Allow · Allow · ✕.
    Request {
        title: String,
        phone: bool,
        devices: Vec<String>,
    },
}

pub(crate) fn top(setup: &Setup, peers: &[Peer], remembered: &Remembered) -> Top {
    if let Some(record) = setup
        .readmissions
        .iter()
        .find(|r| r.error.is_some() || !remembered.closed(&readmission_key(&r.key)))
    {
        return Top::Readmit {
            record: Box::new(record.clone()),
        };
    }
    if let Some(conflict) = setup
        .conflicts
        .iter()
        .find(|c| !remembered.closed(&format!("conflict/{}/{}", c.device_id, c.fingerprint)))
    {
        return Top::Diagnostic {
            title: conflict.diagnostic.clone(),
            key: format!("conflict/{}/{}", conflict.device_id, conflict.fingerprint),
        };
    }
    let name = |device: &str| {
        peers
            .iter()
            .find(|p| p.id == device)
            .map(|p| p.name.clone())
    };
    let requests: Vec<(&Link, String, bool)> = setup
        .requests()
        .into_iter()
        .filter_map(|l| {
            let peer = peers.iter().find(|p| p.id == l.device)?;
            Some((l, peer.name.clone(), peer.phone))
        })
        .collect();
    // Closing the card leaves the request waiting until Pneu restarts. A
    // device that asks later shows the card again, naming everyone waiting.
    if requests
        .iter()
        .any(|(l, _, _)| !remembered.closed(&request_key(setup, l)))
    {
        let names: Vec<String> = requests.iter().map(|(_, n, _)| n.clone()).collect();
        let verb = if names.len() > 1 { "want" } else { "wants" };
        return Top::Request {
            title: format!("{} {verb} to link your devices", askers(&names)),
            phone: requests[0].2,
            devices: requests.iter().map(|(l, _, _)| l.device.clone()).collect(),
        };
    }
    // While a request waits, Link would only answer it; its page has Allow.
    if !requests.is_empty() || setup.links.iter().any(Link::allowing) {
        return Top::None;
    }
    match remembered.offer.as_deref() {
        Some(LINKED) => {
            let names: Vec<String> = setup
                .unanswered()
                .iter()
                .filter_map(|l| name(&l.device))
                .collect();
            if names.is_empty() {
                Top::None
            } else {
                Top::Waiting {
                    title: waiting_on(&names),
                }
            }
        }
        Some(_) => Top::None,
        None => setup
            .undecided()
            .iter()
            .find(|l| name(&l.device).is_some())
            .map(|l| Top::Offer {
                device: l.device.clone(),
            })
            .unwrap_or_default(),
    }
}

/// A request is the same until either generation changes.
pub(crate) fn request_key(setup: &Setup, link: &Link) -> String {
    let binding = setup
        .binding_generations
        .get(&link.device)
        .copied()
        .flatten()
        .unwrap_or_default();
    format!("{}/{}/{binding}", link.device, setup.generation)
}

// ---------- Link request notifications ----------

/// Authority captured when the notification is posted, just like the top card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decision {
    pub readmission: Option<crate::store::members::ReadmissionKey>,
    pub device: pneu_proto::Id,
    pub setup_generation: u64,
    pub binding_generation: u64,
}

pub(crate) async fn decide(core: &crate::core::Core, requests: &[Decision], allow: bool) {
    if let Err(error) = decide_all(core, requests, allow).await {
        eprintln!("pneud: link request action: {error:#}");
    }
}
pub(crate) async fn decide_all(
    core: &crate::core::Core,
    requests: &[Decision],
    allow: bool,
) -> anyhow::Result<()> {
    let _batch = core.setup_decisions.lock().await;
    let mut failure = None;
    for request in requests {
        let result = if let Some(key) = &request.readmission {
            core.decide_readmission(key.clone(), allow).await
        } else {
            core.decide_setup(
                request.device.clone(),
                request.setup_generation,
                request.binding_generation,
                allow,
            )
            .await
        };
        if let Err(error) = result {
            if request.readmission.is_some() {
                core.request_window(false);
            }
            failure = Some(error);
        }
    }
    if let Some(error) = failure {
        return Err(error);
    }
    Ok(())
}

/// A dismissal is remembered by keeping the displayed set. Only a changed set
/// posts again. Answering in the window withdraws the outstanding native notice.
#[derive(Default)]
pub(crate) struct Requests {
    displayed: Vec<Decision>,
    id: u32,
}
impl Requests {
    pub(crate) async fn sync(
        &mut self,
        core: &crate::core::Core,
        actions: &crate::actions::DesktopActions,
    ) {
        self.sync_notice(core.setup_request_notice().await, actions)
            .await;
    }
    pub(crate) async fn sync_notice(
        &mut self,
        notice: Option<super::notifications::Notice>,
        actions: &crate::actions::DesktopActions,
    ) {
        let requests = notice
            .as_ref()
            .map(|n| n.setup_requests.clone())
            .unwrap_or_default();
        if requests == self.displayed {
            return;
        }
        if let Some(notice) = notice {
            match actions
                .notice(notice, Vec::new(), None, self.id, None)
                .await
            {
                Ok(id) => {
                    self.id = id;
                    self.displayed = requests;
                }
                Err(error) => eprintln!("pneud: link request notification: {error:#}"),
            }
        } else {
            if self.id != 0
                && let Err(error) = actions.withdraw_notice(self.id).await
            {
                eprintln!("pneud: withdraw link request: {error:#}");
                return;
            }
            self.id = 0;
            self.displayed.clear();
        }
    }
}

// ---------- "{device} joined" notifications ----------

static CLAIMED: std::sync::Mutex<Vec<(String, u64)>> = std::sync::Mutex::new(Vec::new());

/// One durable joined notice with an in-flight claim.
pub(crate) struct Joined {
    pub device: pneu_proto::Id,
    pub generation: u64,
    pub notice: super::notifications::Notice,
    posted: bool,
}
impl Joined {
    pub(crate) async fn posted(mut self, core: &crate::core::Core, result: anyhow::Result<u32>) {
        match result {
            Ok(_) => {
                if let Err(error) = core
                    .dismiss_setup_notice(self.device.clone(), self.generation)
                    .await
                {
                    eprintln!("pneud: joined notice: {error:#}");
                } else {
                    self.posted = true;
                }
            }
            Err(error) => eprintln!("pneud: notification unavailable: {error:#}"),
        }
    }
}
impl Drop for Joined {
    fn drop(&mut self) {
        if self.posted {
            return;
        }
        CLAIMED
            .lock()
            .expect("joined notices")
            .retain(|(device, generation)| {
                device != self.device.as_str() || *generation != self.generation
            });
    }
}

/// Durable notices are already classified by the core. Failed or cancelled
/// posts release their claims for the next startup, Paired event or ready child.
pub(crate) async fn joined(core: &crate::core::Core) -> Vec<Joined> {
    let setup = match core.setup_status().await {
        Ok(value) => Setup::parse(&serde_json::json!({ "setup": value })),
        Err(error) => {
            eprintln!("pneud: setup status: {error:#}");
            return Vec::new();
        }
    };
    let mut claimed = CLAIMED.lock().expect("joined notices");
    let mut list = Vec::new();
    for n in &setup.notices {
        let key = (n.device.clone(), n.binding_generation);
        let Ok(device) = pneu_proto::Id::new(n.device.as_str()) else {
            continue;
        };
        if claimed.contains(&key) {
            continue;
        }
        claimed.push(key);
        list.push(Joined {
            notice: super::notifications::Notice::joined(
                device.clone(),
                &n.name,
                &n.introducer_name,
            ),
            device,
            generation: n.binding_generation,
            posted: false,
        });
    }
    // Keep successful claims briefly too: a concurrent status read may have
    // captured the row before it was dismissed.
    let excess = claimed.len().saturating_sub(256);
    claimed.drain(..excess);
    list
}

// ---------- What the window remembers between runs ----------

pub(crate) const LINKED: &str = "linked";
pub(crate) const DISMISSED: &str = "dismissed";
pub(crate) const DONE: &str = "done";

/// Window-local choices: the offer's ✕ (for good), the requests' ✕ (until
/// Pneu restarts) and which removal was already shown.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Remembered {
    /// None until Link or ✕ on the offer: "linked", "dismissed" or "done".
    #[serde(default)]
    pub offer: Option<String>,
    /// The daemon run the closed requests belong to.
    #[serde(default)]
    pub daemon: String,
    #[serde(default)]
    pub closed: Vec<String>,
    /// The setup generation whose removal this device already said.
    #[serde(default)]
    pub removed: Option<u64>,
}
impl Remembered {
    pub(crate) fn closed(&self, key: &str) -> bool {
        self.daemon == daemon_instance() && self.closed.iter().any(|k| k == key)
    }
    pub(crate) fn close(&mut self, keys: impl IntoIterator<Item = String>) {
        if self.daemon != daemon_instance() {
            self.daemon = daemon_instance();
            self.closed.clear();
        }
        for key in keys {
            if !self.closed.contains(&key) {
                self.closed.push(key);
            }
        }
        // Bounded by the 16 direct peers, with room for replaced bindings.
        let excess = self.closed.len().saturating_sub(64);
        self.closed.drain(..excess);
    }
    pub(crate) fn load(path: &Path) -> Self {
        use std::io::Read;
        let read = || -> Option<Self> {
            let folder = crate::storage::Folder::open(path.parent()?, false).ok()?;
            let file =
                crate::platform::fs::open_journal_at(&folder.file, path.file_name()?).ok()?;
            let mut bytes = Vec::new();
            file.take(16385).read_to_end(&mut bytes).ok()?;
            if bytes.len() > 16384 {
                return None;
            }
            serde_json::from_slice(&bytes).ok()
        };
        read().unwrap_or_default()
    }
    pub(crate) fn save(&self, path: &Path) -> anyhow::Result<()> {
        crate::config::atomic_private(path, &serde_json::to_vec(self)?)
    }
}

/// One identifier per daemon run. The daemon passes its own to the window
/// processes it starts; macOS hosts the window in the daemon itself.
pub(crate) fn daemon_instance() -> String {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE
        .get_or_init(|| {
            std::env::var("PNEU_DAEMON_INSTANCE")
                .ok()
                .filter(|v| !v.is_empty() && v.len() <= 64)
                .unwrap_or_else(|| crate::random_hex(16))
        })
        .clone()
}

pub(crate) fn readmission_key(key: &crate::store::members::ReadmissionKey) -> String {
    serde_json::to_string(key).expect("re-admission key")
}
pub(crate) fn readmission_text(record: &crate::store::members::Readmission) -> String {
    format!(
        "{} was removed from {}. {} added it again",
        display_peer_text(&record.removed_name),
        super::this_computer(),
        display_peer_text(&record.introducer_name)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn link(device: &str, scope: &str) -> Value {
        json!({"device": device, "scope": scope, "prompt": false, "negotiated": true, "participating": false, "hello_seen": true})
    }
    fn setup(value: Value) -> Setup {
        Setup::parse(&json!({ "setup": value }))
    }

    #[test]
    fn regression_review_dismissed_readmission_shows_later_action_error() {
        let device = pneu_proto::Id::from_bytes([3; 16]);
        let mut setup = setup(json!({"readmissions": [{
            "key": {"device": device, "fingerprint": "a".repeat(64), "removal_hlc": 10,
                "removal_writer": device, "setup_generation": 1},
            "removed_name": "Phone", "value": {"device_id": device,
                "fingerprint": "a".repeat(64), "role": "phone", "name": "Phone",
                "status": "member", "hlc": 20, "writer_device": device},
            "introducer": device, "introducer_name": "Laptop", "decision": "pending"
        }]}));
        assert_eq!(setup.readmissions.len(), 1);
        assert!(matches!(
            top(&setup, &[], &Remembered::default()),
            Top::Readmit { .. }
        ));
        let mut remembered = Remembered::default();
        remembered.close([readmission_key(&setup.readmissions[0].key)]);
        assert_eq!(top(&setup, &[], &remembered), Top::None);
        setup.readmissions[0].error = Some("setup capacity exceeded".into());
        let Top::Readmit { record } = top(&setup, &[], &remembered) else {
            panic!("native action failure must reopen the dismissed card");
        };
        assert_eq!(record.error.as_deref(), Some("setup capacity exceeded"));
        setup.readmissions[0].error = None;
        assert_eq!(top(&setup, &[], &remembered), Top::None);
    }

    #[test]
    fn names_follow_the_copy_rules() {
        let n = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(askers(&n(&["Galaxy S25+"])), "Galaxy S25+");
        assert_eq!(
            askers(&n(&["Galaxy S25+", "MJStanton-Desktop"])),
            "Galaxy S25+ and MJStanton-Desktop"
        );
        assert_eq!(askers(&n(&["A", "B", "C"])), "A and 2 other devices");
        assert_eq!(waiting_on(&n(&["A", "B"])), "Allow linking on A and B");
        assert_eq!(
            waiting_on(&n(&["A", "B", "C"])),
            "Allow linking on 3 devices"
        );
        assert_eq!(also_linked(&n(&["A"])), "Also linked: A");
        assert_eq!(also_linked(&n(&["A", "B"])), "Also linked: A and B");
        assert_eq!(
            also_linked(&n(&["A", "B", "C", "D"])),
            "Also linked: A, B and\u{a0}2\u{a0}more"
        );
        assert_eq!(
            also_linked(&n(&["A", "B", "C"])),
            "Also linked: A, B and\u{a0}1\u{a0}more"
        );
        assert_eq!(also_linked(&[]), "");
    }

    #[test]
    fn pending_allow_hides_offer_for_other_undecided_links() {
        let s = setup(json!({"links": [
            {"device": "aa", "scope": "ask", "prompt": true, "outcome": ALLOWING, "negotiated": true},
            link("bb", "ask")
        ]}));
        let snapshot: super::super::Snapshot = serde_json::from_value(json!({
            "phone": {"name":"", "last_seen":0, "route":""}, "paired":true,
            "connected":false,"paused":false,"receiving":false,"pairing":null,"error":null,
            "devices":[{"device":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","name":"Mac", "role":"computer","state":"connected","last_seen":1,"dialer":null}]
        })).unwrap();
        let mut s = s;
        s.links[1].device = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
        let peers = super::super::window::peers(&snapshot, &[]);
        assert_eq!(top(&s, &peers, &Remembered::default()), Top::None);
    }

    #[test]
    fn requests_offers_and_waiting_lists_follow_link_state() {
        let mut asked = link("aa", "ask");
        asked["prompt"] = json!(true);
        let mut allowing = link("bb", "ask");
        allowing["prompt"] = json!(true);
        allowing["outcome"] = json!(ALLOWING);
        let mut separate = link("cc", "disabled");
        separate["outcome"] = json!(SEPARATE);
        let mut legacy = link("dd", "ask");
        legacy["negotiated"] = json!(false);
        let mut declined = link("ee", "enabled");
        declined["remote_scope"] = json!("disabled");
        let s = setup(json!({"generation": 3, "links": [
            asked, allowing, separate, legacy, link("ff", "ask"), link("gg", "enabled"), declined,
        ], "binding_generations": {"aa": 7}}));
        let ids = |v: Vec<&Link>| v.iter().map(|l| l.device.clone()).collect::<Vec<_>>();
        assert_eq!(ids(s.requests()), ["aa"]);
        assert_eq!(ids(s.undecided()), ["ff"]);
        assert_eq!(ids(s.unanswered()), ["gg"]);
        assert_eq!(request_key(&s, s.link("aa").unwrap()), "aa/3/7");
    }

    #[test]
    fn removal_captures_the_member_as_shown() {
        let s = setup(json!({"generation": 4,
            "roster": [
                {"device_id": "aa", "fingerprint": "f1", "name": "Mac", "status": "member"},
                {"device_id": "bb", "fingerprint": "f2", "name": "Old", "status": "removed"},
            ],
            "binding_generations": {"aa": 2}}));
        assert_eq!(
            s.removal("aa"),
            Some(Removal {
                device: "aa".into(),
                setup_generation: 4,
                binding_generation: Some(2),
                fingerprint: "f1".into(),
            })
        );
        assert!(s.removal("bb").is_none());
        assert!(!s.full());
        assert_eq!(s.learned(&HashSet::new(), &["aa"]), Vec::<String>::new());
    }

    #[test]
    fn remembered_requests_reset_with_the_daemon_and_stay_bounded() {
        let mut r = Remembered::default();
        r.close(["a".to_string()]);
        assert!(r.closed("a"));
        r.daemon = "an earlier run".into();
        assert!(!r.closed("a"));
        r.close((0..100).map(|n| n.to_string()));
        assert_eq!(r.closed.len(), 64);
        assert!(r.closed("99") && !r.closed("a"));
    }
}
