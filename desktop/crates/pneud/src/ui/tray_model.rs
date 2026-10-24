//! Shared menu-bar/tray state projection.
use super::{
    Snapshot,
    window::{Peer, contract::Transfer, display_peer_text},
};
/// The tray's first line, which also starts its tooltip.
pub(crate) fn status(snapshot: &Snapshot, peers: &[Peer], transfers: &[Transfer]) -> String {
    let sending: Vec<&str> = peers
        .iter()
        .filter(|p| p.sending())
        .map(|p| p.name.as_str())
        .collect();
    let receiving = peers
        .iter()
        .find(|p| p.receiving())
        .map(|p| p.name.clone())
        .or_else(|| {
            (snapshot.receiving && snapshot.connected)
                .then(|| display_peer_text(&snapshot.phone.name))
        });
    let connected: Vec<&Peer> = peers.iter().filter(|p| p.connected()).collect();
    if let Some(error) = &snapshot.error {
        error.tooltip()
    } else if let Some(name) = &snapshot.pairing {
        format!("Pairing with {}…", display_peer_text(name))
    } else if peers.is_empty() {
        "No devices paired".into()
    } else if let [one] = sending.as_slice() {
        format!("Sending to {one}…")
    } else if !sending.is_empty() {
        format!("Sending to {} devices…", sending.len())
    } else if let Some(name) = receiving {
        format!("Receiving from {name}…")
    } else if let [one] = peers {
        // One device keeps v2's wording.
        if one.paused() {
            format!("{} paused sending", one.name)
        } else if let (true, Some(route)) = (one.connected(), &one.route) {
            format!("{} connected over {route}", one.name)
        } else if one.connected() {
            format!("{} connected", one.name)
        } else if one.lost() {
            format!(
                "{} doesn't recognize {}",
                one.name,
                super::window::this_computer()
            )
        } else {
            format!("{} isn't connected", one.name)
        }
    } else if connected.len() == 2 && peers.len() == 2 {
        format!("{} and {} connected", connected[0].name, connected[1].name)
    } else if connected.is_empty() {
        let _ = transfers;
        "No devices connected".into()
    } else {
        format!("{} of {} devices connected", connected.len(), peers.len())
    }
}
pub(crate) fn icon(snapshot: &Snapshot, peers: &[Peer]) -> &'static str {
    if snapshot.error.is_some() {
        "pneu-tray-error"
    } else if peers.iter().any(Peer::paused) {
        "pneu-tray-paused"
    } else if peers.iter().any(|p| p.sending() || p.receiving())
        || (snapshot.receiving && snapshot.connected)
    {
        "pneu-tray-receiving"
    } else if peers.iter().any(Peer::connected) {
        "pneu-tray-connected"
    } else {
        "pneu-tray-disconnected"
    }
}
