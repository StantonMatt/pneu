//! One route-aware candidate list for QR endpoints and mDNS advertisement.
use anyhow::Result;
use std::{collections::HashSet, net::IpAddr, path::Path};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InterfaceAddress {
    pub name: String,
    pub ip: IpAddr,
    pub physical: bool,
    pub up: bool,
}

pub(crate) fn candidates() -> Result<Vec<InterfaceAddress>> {
    let routes = default_routes(
        &route_table("/proc/net/route"),
        &route_table("/proc/net/ipv6_route"),
    );
    let interfaces = if_addrs::get_if_addrs()?
        .into_iter()
        .map(|i| InterfaceAddress {
            ip: i.ip(),
            physical: Path::new("/sys/class/net")
                .join(&i.name)
                .join("device")
                .exists(),
            up: i.oper_status == if_addrs::IfOperStatus::Up,
            name: i.name,
        })
        .collect();
    Ok(prioritize(interfaces, &routes))
}
fn route_table(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(table) => table,
        Err(error) => {
            // Discovery is an optional routing hint. Physical LAN candidates
            // still work if route information is unavailable.
            eprintln!("pneud: route discovery unavailable ({path}): {error}");
            String::new()
        }
    }
}
fn default_routes(v4: &str, v6: &str) -> HashSet<String> {
    let mut routes = HashSet::new();
    for line in v4.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 8
            && fields[1] == "00000000"
            && fields[7] == "00000000"
            && route_is_up(fields[3])
        {
            routes.insert(fields[0].to_owned());
        }
    }
    for line in v6.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() >= 10
            && fields[0] == "00000000000000000000000000000000"
            && fields[1] == "00"
            && route_is_up(fields[8])
        {
            routes.insert(fields[9].to_owned());
        }
    }
    routes
}
fn route_is_up(flags: &str) -> bool {
    u32::from_str_radix(flags, 16).is_ok_and(|flags| flags & 1 != 0 && flags & 0x200 == 0)
}
fn tailscale(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(v) if v.octets()[0] == 100 && (64..=127).contains(&v.octets()[1]))
}
fn excluded(i: &InterfaceAddress) -> bool {
    !i.up
        || i.name == "lo"
        || ["docker", "br-", "veth", "virbr"]
            .iter()
            .any(|prefix| i.name.starts_with(prefix))
        || i.ip.is_loopback()
        || i.ip.is_unspecified()
        || i.ip.is_multicast()
        || match i.ip {
            IpAddr::V4(ip) => ip.is_link_local() || ip.is_broadcast(),
            IpAddr::V6(ip) => ip.is_unicast_link_local(),
        }
}
fn prioritize(
    mut interfaces: Vec<InterfaceAddress>,
    defaults: &HashSet<String>,
) -> Vec<InterfaceAddress> {
    interfaces
        .retain(|i| !excluded(i) && (tailscale(i.ip) || defaults.contains(&i.name) || i.physical));
    interfaces.sort_by_key(|i| {
        (
            if tailscale(i.ip) {
                0
            } else if defaults.contains(&i.name) {
                1
            } else {
                2
            },
            i.ip.is_ipv6(),
            i.name.clone(),
            i.ip,
        )
    });
    let mut seen = HashSet::new();
    interfaces.retain(|i| seen.insert(i.ip));
    // Use the same shortlist for mDNS and the eight-slot pairing URI. DNS A/AAAA
    // records are unordered, so selection priority is not a wire-order promise.
    interfaces.truncate(8);
    interfaces
}

#[cfg(test)]
mod tests {
    use super::*;
    fn address(name: &str, ip: &str, physical: bool) -> InterfaceAddress {
        InterfaceAddress {
            name: name.into(),
            ip: ip.parse().unwrap(),
            physical,
            up: true,
        }
    }
    #[test]
    fn qr_and_mdns_shortlist_prioritize_uplinks_and_exclude_virtual_and_link_local() {
        let mut interfaces = vec![
            address("wlo1", "192.168.1.20", true),
            address("enp4s0", "10.0.0.10", true),
            address("tailscale0", "100.67.144.28", false),
            address("enp9s0", "10.20.0.10", true),
            address("tun0", "10.40.0.10", false),
            address("lo", "127.0.0.1", true),
            address("enp4s0", "169.254.1.1", true),
            address("enp4s0", "fe80::1", true),
            address("enp4s0", "2001:db8::1", true),
            address("veth42", "10.80.0.1", true),
            address("virbr0", "192.168.122.1", true),
        ];
        for n in 0..12 {
            interfaces.push(address(
                &format!("br-{n}"),
                &format!("172.{}.0.1", 18 + n),
                true,
            ));
        }
        interfaces.push(address("docker0", "172.17.0.1", true));
        interfaces.push(address("wlo1", "192.168.1.20", true));
        let defaults = ["wlo1".into(), "enp4s0".into(), "docker0".into()]
            .into_iter()
            .collect();
        let expected: Vec<IpAddr> = [
            "100.67.144.28",
            "10.0.0.10",
            "192.168.1.20",
            "2001:db8::1",
            "10.20.0.10",
        ]
        .into_iter()
        .map(|ip| ip.parse().unwrap())
        .collect();
        let first = prioritize(interfaces.clone(), &defaults);
        assert_eq!(first.iter().map(|i| i.ip).collect::<Vec<_>>(), expected);
        interfaces.reverse();
        assert_eq!(prioritize(interfaces, &defaults), first);
        // Without Tailscale, the actual Wi-Fi uplink still survives truncation.
        assert!(first.iter().any(|i| i.name == "wlo1"));
    }
    #[test]
    fn default_routes_include_both_families_but_ignore_rejected_and_specific_routes() {
        let v4 = "Iface Destination Gateway Flags RefCnt Use Metric Mask\n\
            wlo1 00000000 0101A8C0 0003 0 0 600 00000000\n\
            enp4s0 00000000 0100000A 0003 0 0 100 00000000\n\
            docker0 000011AC 00000000 0001 0 0 0 0000FFFF\n";
        let v6 = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000064 0 0 00000003 enp5s0\n\
            00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 0 0 00200200 lo\n";
        assert_eq!(
            default_routes(v4, v6),
            ["wlo1".into(), "enp4s0".into(), "enp5s0".into()]
                .into_iter()
                .collect()
        );
    }
    #[test]
    fn eight_slot_limit_keeps_tailscale_and_defaults_ahead_of_other_physical_lans() {
        let mut interfaces: Vec<_> = (0..12)
            .map(|n| address(&format!("enp{n}s0"), &format!("10.{}.0.1", n + 1), true))
            .collect();
        interfaces.push(address("wlo1", "192.168.1.20", true));
        let defaults = ["wlo1".into()].into_iter().collect();
        let lan_only = prioritize(interfaces.clone(), &defaults);
        assert_eq!(lan_only.len(), 8);
        assert_eq!(lan_only[0].name, "wlo1");
        interfaces.push(address("tailscale0", "100.64.1.1", false));
        let result = prioritize(interfaces, &defaults);
        assert_eq!(result.len(), 8);
        assert_eq!(result[0].name, "tailscale0");
        assert_eq!(result[1].name, "wlo1");
    }
}
