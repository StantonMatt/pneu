//! Route-aware discovery, with a compact, interface-diverse pairing shortlist.
use anyhow::Result;
use std::{collections::HashSet, net::IpAddr, path::Path};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct InterfaceAddress {
    pub name: String,
    pub ip: IpAddr,
    pub physical: bool,
    pub up: bool,
    pub temporary: bool,
}

pub(crate) fn candidates() -> Result<Vec<InterfaceAddress>> {
    let routes = default_routes(
        &network_table("/proc/net/route"),
        &network_table("/proc/net/ipv6_route"),
    );
    let temporary = temporary_ipv6_addresses(&network_table("/proc/net/if_inet6"));
    let interfaces = if_addrs::get_if_addrs()?
        .into_iter()
        .map(|i| InterfaceAddress {
            ip: i.ip(),
            physical: Path::new("/sys/class/net")
                .join(&i.name)
                .join("device")
                .exists(),
            up: i.oper_status == if_addrs::IfOperStatus::Up,
            temporary: temporary.contains(&(i.name.clone(), i.ip())),
            name: i.name,
        })
        .collect();
    Ok(prioritize(interfaces, &routes))
}
pub(crate) fn pairing_candidates() -> Result<Vec<InterfaceAddress>> {
    Ok(pairing_shortlist(candidates()?))
}
fn pairing_shortlist(interfaces: Vec<InterfaceAddress>) -> Vec<InterfaceAddress> {
    // Three rounds, capped at three total addresses:
    // 1. One best address per interface: IPv4, else global stable IPv6 preferred.
    //    candidates() orders interfaces Tailscale -> default-route -> other physical.
    // 2. Each dual-stack interface's best global, non-temporary IPv6, in that order.
    // 3. Remaining aliases. mDNS keeps every eligible address.
    let mut seen = HashSet::new();
    let (distinct, mut aliases): (Vec<_>, Vec<_>) = interfaces
        .into_iter()
        .partition(|i| seen.insert(i.name.clone()));
    let mut dual_stack = Vec::new();
    for primary in &distinct {
        if primary.ip.is_ipv4()
            && let Some(index) = aliases
                .iter()
                .position(|i| i.name == primary.name && global_ipv6(i.ip) && !i.temporary)
        {
            dual_stack.push(aliases.remove(index));
        }
    }
    distinct
        .into_iter()
        .chain(dual_stack)
        .chain(aliases)
        .take(3)
        .collect()
}
fn network_table(path: &str) -> String {
    match std::fs::read_to_string(path) {
        Ok(table) => table,
        Err(error) => {
            // Discovery is an optional routing hint. Physical LAN candidates
            // still work if route information is unavailable.
            eprintln!("pneud: network discovery unavailable ({path}): {error}");
            String::new()
        }
    }
}
fn temporary_ipv6_addresses(table: &str) -> HashSet<(String, IpAddr)> {
    table
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() != 6 || fields[0].len() != 32 {
                return None;
            }
            let flags = u32::from_str_radix(fields[4], 16).ok()?;
            // IFA_F_TEMPORARY (0x01) is an address flag, not an interface flag.
            if flags & 1 == 0 {
                return None;
            }
            let ip = std::net::Ipv6Addr::from(u128::from_str_radix(fields[0], 16).ok()?);
            Some((fields[5].to_owned(), IpAddr::V6(ip)))
        })
        .collect()
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
fn global_ipv6(ip: IpAddr) -> bool {
    // Global unicast space is 2000::/3; ULA addresses remain lower-priority aliases.
    matches!(ip, IpAddr::V6(ip) if ip.segments()[0] & 0xe000 == 0x2000)
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
            i.ip.is_ipv6() && !global_ipv6(i.ip),
            i.temporary,
            i.name.clone(),
            i.ip,
        )
    });
    let mut seen = HashSet::new();
    interfaces.retain(|i| seen.insert(i.ip));
    // mDNS advertises every eligible address; only the pairing URI is capped.
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
            temporary: false,
        }
    }
    fn desktop_interfaces() -> Vec<InterfaceAddress> {
        vec![
            address("tailscale0", "100.67.144.28", false),
            address("enp4s0", "10.10.10.10", true),
            address("wlo1", "10.10.20.13", true),
            address("docker0", "172.17.0.1", true),
            address("br-a", "172.18.0.1", true),
            address("br-b", "172.19.0.1", true),
            address("enp4s0", "2001:db8::20", true),
            address("enp4s0", "2001:db8::30", true),
            InterfaceAddress {
                temporary: true,
                ..address("enp4s0", "2001:db8::1", true)
            },
            InterfaceAddress {
                temporary: true,
                ..address("enp4s0", "2001:db8::2", true)
            },
        ]
    }
    fn desktop_defaults() -> HashSet<String> {
        ["enp4s0".into(), "wlo1".into()].into_iter().collect()
    }
    fn ips(interfaces: &[InterfaceAddress]) -> Vec<IpAddr> {
        interfaces.iter().map(|i| i.ip).collect()
    }
    #[test]
    fn desktop_pairing_keeps_tailscale_ethernet_and_wifi_before_ethernet_aliases() {
        for defaults in [desktop_defaults(), ["enp4s0".into()].into_iter().collect()] {
            let mut interfaces = desktop_interfaces();
            interfaces.push(address("enp4s0", "10.10.10.11", true));
            let advertised = prioritize(interfaces.clone(), &defaults);
            // Every eligible alias stays in mDNS, including all four global IPv6 addresses.
            assert_eq!(advertised.len(), 8);
            let selected = pairing_shortlist(advertised);
            assert_eq!(
                ips(&selected),
                ["100.67.144.28", "10.10.10.10", "10.10.20.13"]
                    .map(|ip| ip.parse::<IpAddr>().unwrap())
            );
            interfaces.reverse();
            assert_eq!(
                pairing_shortlist(prioritize(interfaces, &defaults)),
                selected
            );
        }
    }
    #[test]
    fn pairing_fills_unused_slots_with_ipv4_aliases() {
        let mut interfaces = desktop_interfaces();
        interfaces.retain(|i| i.name != "wlo1" && i.ip.is_ipv4());
        interfaces.push(address("enp4s0", "10.10.10.11", true));
        assert_eq!(
            ips(&pairing_shortlist(prioritize(
                interfaces,
                &desktop_defaults()
            ))),
            ["100.67.144.28", "10.10.10.10", "10.10.10.11"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
    }
    #[test]
    fn tailscale_ipv4_does_not_suppress_stable_ipv6_lan_fallback() {
        let mut interfaces = desktop_interfaces();
        interfaces.retain(|i| !i.ip.is_ipv4() || tailscale(i.ip));
        assert_eq!(
            ips(&pairing_shortlist(prioritize(
                interfaces,
                &desktop_defaults()
            ))),
            ["100.67.144.28", "2001:db8::20", "2001:db8::30"]
                .map(|ip| ip.parse::<IpAddr>().unwrap())
        );
    }
    #[test]
    fn dual_stack_interface_adds_stable_ipv6_before_remaining_aliases() {
        let mut interfaces = desktop_interfaces();
        interfaces.retain(|i| i.name == "enp4s0");
        // Round 2 must beat this lower-sorted IPv4 alias.
        interfaces.push(address("enp4s0", "10.10.10.11", true));
        let selected = pairing_shortlist(prioritize(interfaces, &desktop_defaults()));
        assert_eq!(
            ips(&selected),
            ["10.10.10.10", "2001:db8::20", "10.10.10.11"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
        let single = vec![
            address("enp4s0", "10.10.10.10", true),
            address("enp4s0", "2001:db8::20", true),
        ];
        assert_eq!(
            ips(&pairing_shortlist(prioritize(single, &desktop_defaults()))),
            ["10.10.10.10", "2001:db8::20"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
    }
    #[test]
    fn mixed_lans_keep_ipv6_only_wifi_before_ipv4_ethernet_aliases() {
        for defaults in [desktop_defaults(), ["enp4s0".into()].into_iter().collect()] {
            let mut interfaces = desktop_interfaces();
            interfaces.retain(|i| i.name != "wlo1");
            interfaces.extend([
                address("enp4s0", "10.10.10.11", true),
                address("wlo1", "fd12::1", true),
                address("wlo1", "2001:db8::40", true),
                InterfaceAddress {
                    temporary: true,
                    ..address("wlo1", "2001:db8::1", true)
                },
            ]);
            let selected = pairing_shortlist(prioritize(interfaces.clone(), &defaults));
            assert_eq!(
                ips(&selected),
                ["100.67.144.28", "10.10.10.10", "2001:db8::40"]
                    .map(|ip| ip.parse::<IpAddr>().unwrap())
            );
            interfaces.reverse();
            assert_eq!(
                pairing_shortlist(prioritize(interfaces, &defaults)),
                selected
            );
        }
    }
    #[test]
    fn mixed_lans_keep_ipv6_only_wifi_then_add_dual_stack_ethernet_ipv6() {
        let mut interfaces = vec![
            address("enp4s0", "10.10.10.10", true),
            address("enp4s0", "2001:db8::20", true),
            address("wlo1", "2001:db8::40", true),
        ];
        let defaults = desktop_defaults();
        assert_eq!(
            ips(&pairing_shortlist(prioritize(
                interfaces.clone(),
                &defaults
            ))),
            ["10.10.10.10", "2001:db8::40", "2001:db8::20"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
        interfaces.push(address("enp4s0", "10.10.10.11", true));
        assert_eq!(
            ips(&pairing_shortlist(prioritize(interfaces, &defaults))),
            ["10.10.10.10", "2001:db8::40", "2001:db8::20"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
    }
    #[test]
    fn dual_stack_round_follows_primary_interface_order_before_aliases() {
        let mut interfaces = desktop_interfaces();
        interfaces.retain(|i| i.name != "tailscale0");
        interfaces.extend([
            address("enp4s0", "10.10.10.11", true),
            address("wlo1", "2001:db8::40", true),
        ]);
        for (defaults, expected) in [
            (
                desktop_defaults(),
                ["10.10.10.10", "10.10.20.13", "2001:db8::20"],
            ),
            (
                ["wlo1".into()].into_iter().collect(),
                ["10.10.20.13", "10.10.10.10", "2001:db8::40"],
            ),
        ] {
            let selected = pairing_shortlist(prioritize(interfaces.clone(), &defaults));
            assert_eq!(
                ips(&selected),
                expected.map(|ip| ip.parse::<IpAddr>().unwrap())
            );
            interfaces.reverse();
            assert_eq!(
                pairing_shortlist(prioritize(interfaces.clone(), &defaults)),
                selected
            );
        }
    }
    #[test]
    fn temporary_ipv6_enters_only_as_a_remaining_alias_when_no_stable_global_exists() {
        let interfaces = vec![
            address("enp4s0", "10.10.10.10", true),
            address("enp4s0", "10.10.10.11", true),
            InterfaceAddress {
                temporary: true,
                ..address("enp4s0", "2001:db8::1", true)
            },
        ];
        assert_eq!(
            ips(&pairing_shortlist(prioritize(
                interfaces,
                &desktop_defaults()
            ))),
            ["10.10.10.10", "10.10.10.11", "2001:db8::1"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
    }
    #[test]
    fn ipv6_only_default_route_precedes_other_physical_ipv4_interfaces() {
        let interfaces = vec![
            address("enp4s0", "10.10.10.10", true),
            address("enp4s0", "10.10.10.11", true),
            address("tailscale0", "100.67.144.28", false),
            address("wlo1", "2001:db8::40", true),
        ];
        let defaults = ["wlo1".into()].into_iter().collect();
        assert_eq!(
            ips(&pairing_shortlist(prioritize(interfaces, &defaults))),
            ["100.67.144.28", "2001:db8::40", "10.10.10.10"]
                .map(|ip| ip.parse::<IpAddr>().unwrap())
        );
    }
    #[test]
    fn ipv6_only_pairing_prefers_global_stable_addresses_and_distinct_interfaces() {
        let mut interfaces = desktop_interfaces();
        interfaces.retain(|i| i.ip.is_ipv6());
        interfaces.push(address("enp4s0", "fd12::1", true));
        interfaces.push(address("wlo1", "2001:db8::40", true));
        let defaults = desktop_defaults();
        let selected = pairing_shortlist(prioritize(interfaces.clone(), &defaults));
        assert_eq!(
            ips(&selected),
            ["2001:db8::20", "2001:db8::40", "2001:db8::30"]
                .map(|ip| ip.parse::<IpAddr>().unwrap())
        );
        interfaces.reverse();
        assert_eq!(
            pairing_shortlist(prioritize(interfaces, &defaults)),
            selected
        );
        // With one interface, global temporary addresses precede a local-only ULA.
        interfaces = vec![
            address("enp4s0", "fd12::1", true),
            InterfaceAddress {
                temporary: true,
                ..address("enp4s0", "2001:db8::1", true)
            },
            address("enp4s0", "2001:db8::20", true),
        ];
        assert_eq!(
            ips(&pairing_shortlist(prioritize(interfaces, &defaults))),
            ["2001:db8::20", "2001:db8::1", "fd12::1"].map(|ip| ip.parse::<IpAddr>().unwrap())
        );
    }
    #[test]
    fn linux_ipv6_temporary_flags_are_address_specific_and_malformed_rows_are_ignored() {
        let table = "20010db8000000000000000000000001 02 40 00 01 enp4s0\n\
            20010db8000000000000000000000002 02 40 00 21 enp4s0\n\
            20010db8000000000000000000000001 03 40 00 00 wlo1\n\
            20010db8000000000000000000000003 02 40 00 80 enp4s0\n\
            malformed\n\
            z0010db8000000000000000000000004 02 40 00 01 enp4s0\n\
            20010db8000000000000000000000005 02 40 00 zz enp4s0\n";
        assert_eq!(
            temporary_ipv6_addresses(table),
            [
                ("enp4s0".into(), "2001:db8::1".parse().unwrap()),
                ("enp4s0".into(), "2001:db8::2".parse().unwrap()),
            ]
            .into_iter()
            .collect()
        );
        assert!(temporary_ipv6_addresses("").is_empty());
    }
    #[test]
    fn candidates_prioritize_uplinks_and_exclude_virtual_and_link_local() {
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
        assert_eq!(
            pairing_shortlist(first.clone())
                .iter()
                .map(|i| i.ip)
                .collect::<Vec<_>>(),
            expected[..3]
        );
        interfaces.reverse();
        assert_eq!(prioritize(interfaces, &defaults), first);
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
    fn three_slot_pairing_limit_keeps_tailscale_and_defaults_without_capping_mdns() {
        let mut interfaces: Vec<_> = (0..12)
            .map(|n| address(&format!("enp{n}s0"), &format!("10.{}.0.1", n + 1), true))
            .collect();
        interfaces.push(address("wlo1", "192.168.1.20", true));
        let defaults = ["wlo1".into()].into_iter().collect();
        interfaces.push(address("wlo1", "2001:db8::1", true));
        let advertised = prioritize(interfaces.clone(), &defaults);
        assert_eq!(advertised.len(), 14);
        let lan_only = pairing_shortlist(advertised);
        assert_eq!(lan_only.len(), 3);
        assert_eq!(lan_only[0].name, "wlo1");
        interfaces.push(address("tailscale0", "100.64.1.1", false));
        let result = pairing_shortlist(prioritize(interfaces, &defaults));
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].name, "tailscale0");
        assert_eq!(result[1].name, "wlo1");
        assert!(result.iter().all(|i| i.ip.is_ipv4()));
        assert!(result[2].physical);
    }
    #[test]
    fn pairing_falls_back_to_ipv6_when_lan_has_no_ipv4() {
        let interfaces = vec![
            address("tailscale0", "fd7a:115c:a1e0::1", false),
            address("wlo1", "2001:db8::1", true),
            address("enp4s0", "2001:db8::2", true),
        ];
        let defaults = ["tailscale0".into(), "wlo1".into()].into_iter().collect();
        let advertised = prioritize(interfaces, &defaults);
        assert_eq!(advertised.len(), 3);
        let selected = pairing_shortlist(advertised);
        assert_eq!(selected.len(), 3);
        assert!(selected.iter().all(|i| i.ip.is_ipv6()));
        assert_eq!(selected[0].name, "wlo1");
        assert_eq!(selected[1].name, "tailscale0");
        assert_eq!(selected[2].name, "enp4s0");
    }
    #[test]
    fn tailscale_priority_uses_exact_cgnat_range() {
        assert!(!tailscale("100.63.255.255".parse().unwrap()));
        assert!(tailscale("100.64.0.0".parse().unwrap()));
        assert!(tailscale("100.127.255.255".parse().unwrap()));
        assert!(!tailscale("100.128.0.0".parse().unwrap()));
    }
}
