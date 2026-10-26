use super::{InterfaceAddress, prioritize};
use std::{collections::HashSet, net::IpAddr};

#[derive(Debug)]
struct Adapter {
    luid: u64,
    name: String,
    up: bool,
    interface_flags: u8,
    addresses: Vec<Address>,
}
#[derive(Debug)]
struct Address {
    ip: IpAddr,
    preferred: bool,
    random_suffix: bool,
}

fn map_adapters(adapters: Vec<Adapter>, default_luids: &HashSet<u64>) -> Vec<InterfaceAddress> {
    let defaults = adapters
        .iter()
        .filter(|adapter| default_luids.contains(&adapter.luid))
        .map(|adapter| adapter.name.clone())
        .collect();
    let interfaces = adapters
        .into_iter()
        .flat_map(|adapter| {
            adapter
                .addresses
                .into_iter()
                .filter(|address| address.preferred)
                .map(move |address| {
                    InterfaceAddress {
                        name: adapter.name.clone(),
                        ip: address.ip,
                        up: adapter.up,
                        // MIB_IF_ROW2.HardwareInterface, not IF_TYPE_ETHERNET_CSMACD:
                        // software Ethernet (Hyper-V/WSL) and VPNs are not hardware.
                        physical: adapter.interface_flags & 1 != 0,
                        temporary: address.ip.is_ipv6() && address.random_suffix,
                    }
                })
        })
        .collect();
    prioritize(interfaces, &defaults)
}
fn default_route(destination: IpAddr, prefix: u8, lifetime: u32, loopback: bool) -> bool {
    destination.is_unspecified() && prefix == 0 && lifetime != 0 && !loopback
}

#[cfg(windows)]
pub(crate) fn candidates() -> anyhow::Result<Vec<InterfaceAddress>> {
    let adapters = native::adapters()?;
    // As on Linux, missing routing hints must not hide physical LAN interfaces.
    let defaults = native::default_luids().unwrap_or_else(|error| {
        eprintln!("pneud: default-route discovery unavailable: {error}");
        HashSet::new()
    });
    Ok(map_adapters(adapters, &defaults))
}

#[cfg(windows)]
mod native {
    use super::*;
    use anyhow::{Context, Result, bail};
    use std::{io, mem::size_of, ptr};
    use windows_sys::Win32::{
        Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, NO_ERROR},
        NetworkManagement::{IpHelper::*, Ndis::IfOperStatusUp},
        Networking::WinSock::*,
    };

    // All pointers below refer to live buffers owned by GetAdaptersAddresses
    // or GetIpForwardTable2; copy their contents before releasing those buffers.
    pub(super) fn adapters() -> Result<Vec<Adapter>> {
        let mut size = 15 * 1024u32;
        for _ in 0..4 {
            // The API requires 8-byte alignment, including its linked records.
            let mut buffer = vec![0u64; (size as usize).div_ceil(size_of::<u64>())];
            let head = buffer.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
            let status = unsafe {
                GetAdaptersAddresses(
                    AF_UNSPEC as u32,
                    GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER,
                    ptr::null(),
                    head,
                    &mut size,
                )
            };
            if status == ERROR_BUFFER_OVERFLOW {
                continue;
            }
            if status == ERROR_NO_DATA {
                return Ok(Vec::new());
            }
            if status != NO_ERROR {
                return Err(io::Error::from_raw_os_error(status as i32))
                    .context("GetAdaptersAddresses");
            }
            let mut adapters = Vec::new();
            let mut next = head;
            while let Some(adapter) = unsafe { next.as_ref() } {
                let mut row = MIB_IF_ROW2 {
                    InterfaceLuid: adapter.Luid,
                    ..Default::default()
                };
                let status = unsafe { GetIfEntry2(&mut row) };
                let interface_flags = if status == NO_ERROR {
                    row.InterfaceAndOperStatusFlags._bitfield
                } else {
                    eprintln!(
                        "pneud: interface metadata unavailable: {}",
                        io::Error::from_raw_os_error(status as i32)
                    );
                    0
                };
                let mut name = Vec::new();
                if !adapter.FriendlyName.is_null() {
                    let mut p = adapter.FriendlyName;
                    // The API supplies a NUL-terminated UTF-16 friendly name.
                    unsafe {
                        while *p != 0 {
                            name.push(*p);
                            p = p.add(1);
                        }
                    }
                }
                let mut addresses = Vec::new();
                let mut unicast = adapter.FirstUnicastAddress;
                while let Some(address) = unsafe { unicast.as_ref() } {
                    if let Some(ip) = unsafe { socket_ip(&address.Address) } {
                        addresses.push(Address {
                            ip,
                            preferred: address.DadState == IpDadStatePreferred,
                            // Windows reports randomized Public SLAAC identifiers as
                            // LinkLayerAddress; Random denotes anonymous IPv6 addresses
                            // (MIB_UNICASTIPADDRESS_ROW). TRANSIENT means a cluster address,
                            // and finite lifetimes also occur on Public SLAAC addresses.
                            random_suffix: address.SuffixOrigin == IpSuffixOriginRandom,
                        });
                    }
                    unicast = address.Next;
                }
                adapters.push(Adapter {
                    luid: unsafe { adapter.Luid.Value },
                    name: String::from_utf16_lossy(&name),
                    up: adapter.OperStatus == IfOperStatusUp,
                    interface_flags,
                    addresses,
                });
                next = adapter.Next;
            }
            return Ok(adapters);
        }
        bail!("network adapters changed repeatedly during discovery")
    }

    pub(super) fn default_luids() -> Result<HashSet<u64>> {
        struct Table(*mut MIB_IPFORWARD_TABLE2);
        impl Drop for Table {
            fn drop(&mut self) {
                unsafe { FreeMibTable(self.0.cast()) };
            }
        }
        let mut table = ptr::null_mut();
        let status = unsafe { GetIpForwardTable2(AF_UNSPEC, &mut table) };
        if status != NO_ERROR {
            return Err(io::Error::from_raw_os_error(status as i32)).context("GetIpForwardTable2");
        }
        let table = Table(table);
        let mut defaults = HashSet::new();
        // NumEntries describes the flexible array allocated by the API. Do not
        // index the bindings' one-element placeholder array beyond its length.
        let rows = unsafe {
            std::slice::from_raw_parts(
                ptr::addr_of!((*table.0).Table).cast::<MIB_IPFORWARD_ROW2>(),
                (*table.0).NumEntries as usize,
            )
        };
        for row in rows {
            let prefix = &row.DestinationPrefix;
            let address = SOCKET_ADDRESS {
                lpSockaddr: ptr::from_ref(&prefix.Prefix).cast_mut().cast(),
                iSockaddrLength: size_of::<SOCKADDR_INET>() as i32,
            };
            if unsafe { socket_ip(&address) }.is_some_and(|ip| {
                default_route(ip, prefix.PrefixLength, row.ValidLifetime, row.Loopback)
            }) {
                defaults.insert(unsafe { row.InterfaceLuid.Value });
            }
        }
        Ok(defaults)
    }

    unsafe fn socket_ip(address: &SOCKET_ADDRESS) -> Option<IpAddr> {
        if address.lpSockaddr.is_null() || address.iSockaddrLength < size_of::<u16>() as i32 {
            return None;
        }
        // Read unaligned: SOCKET_ADDRESS does not itself promise sockaddr alignment.
        match unsafe { ptr::read_unaligned(address.lpSockaddr.cast::<u16>()) } {
            AF_INET if address.iSockaddrLength >= size_of::<SOCKADDR_IN>() as i32 => {
                let v4 = unsafe { ptr::read_unaligned(address.lpSockaddr.cast::<SOCKADDR_IN>()) };
                Some(IpAddr::V4(
                    unsafe { v4.sin_addr.S_un.S_addr }.to_ne_bytes().into(),
                ))
            }
            AF_INET6 if address.iSockaddrLength >= size_of::<SOCKADDR_IN6>() as i32 => {
                let v6 = unsafe { ptr::read_unaligned(address.lpSockaddr.cast::<SOCKADDR_IN6>()) };
                Some(IpAddr::V6(unsafe { v6.sin6_addr.u.Byte }.into()))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn adapter(luid: u64, name: &str, flags: u8, ips: &[&str]) -> Adapter {
        Adapter {
            luid,
            name: name.into(),
            up: true,
            interface_flags: flags,
            addresses: ips
                .iter()
                .map(|ip| Address {
                    ip: ip.parse().unwrap(),
                    preferred: true,
                    random_suffix: false,
                })
                .collect(),
        }
    }
    #[test]
    fn mapping_keeps_hardware_routes_and_tailscale_but_not_software_ethernet() {
        let candidates = map_adapters(
            vec![
                adapter(1, "Ethernet", 1, &["192.168.1.2"]),
                adapter(2, "vEthernet (WSL)", 4, &["172.24.0.1"]),
                adapter(3, "vEthernet (Default Switch)", 4, &["172.25.0.1"]),
                adapter(4, "VPN", 0, &["10.1.0.2", "2001:db8::1"]),
                adapter(5, "Tailscale", 0, &["100.67.1.2"]),
            ],
            &[4].into_iter().collect(),
        );
        assert_eq!(
            candidates
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            ["Tailscale", "VPN", "VPN", "Ethernet"]
        );
        assert!(
            candidates
                .iter()
                .all(|i| i.physical == (i.name == "Ethernet"))
        );
    }
    #[test]
    fn mapping_respects_dad_oper_status_and_per_address_ipv6_suffix() {
        let mut lan = adapter(
            1,
            "Wi-Fi",
            1,
            &[
                "192.168.1.2",
                "2001:db8::1",
                "2001:db8::2",
                "2001:db8::3",
                "fe80::1",
            ],
        );
        lan.addresses[0].random_suffix = true; // IPv4 DHCP is not temporary IPv6.
        lan.addresses[1].random_suffix = true;
        lan.addresses[3].preferred = false;
        let mut down = adapter(2, "Ethernet", 1, &["10.0.0.2"]);
        down.up = false;
        let result = map_adapters(vec![lan, down], &HashSet::new());
        assert_eq!(
            result
                .iter()
                .map(|i| (i.ip.to_string(), i.temporary))
                .collect::<Vec<_>>(),
            [
                ("192.168.1.2".into(), false),
                ("2001:db8::2".into(), false),
                ("2001:db8::1".into(), true)
            ]
        );
    }
    #[test]
    fn randomized_public_slaac_remains_the_dual_stack_pairing_fallback() {
        // With RandomizeIdentifiers enabled, Windows still reports Public SLAAC
        // as SuffixOrigin=Link, even with finite preferred and valid lifetimes.
        let mut lan = adapter(
            1,
            "Ethernet",
            1,
            &["192.168.1.2", "2001:db8::1", "2001:db8::ffff"],
        );
        lan.addresses[1].random_suffix = true; // Anonymous/Temporary: Random.
        // The numerically later Public address must precede the temporary alias.
        lan.addresses[2].random_suffix = false; // Randomized Public: Link.
        let other = adapter(2, "Wi-Fi", 1, &["192.168.2.2"]);
        let result =
            super::super::pairing_shortlist(map_adapters(vec![lan, other], &HashSet::new()));
        assert_eq!(
            result.iter().map(|i| i.ip.to_string()).collect::<Vec<_>>(),
            ["192.168.1.2", "192.168.2.2", "2001:db8::ffff"]
        );
        assert!(result.iter().all(|i| !i.temporary));
    }
    #[test]
    fn routes_require_a_live_non_loopback_default_prefix() {
        for ip in ["0.0.0.0", "::"] {
            let ip = ip.parse().unwrap();
            assert!(default_route(ip, 0, u32::MAX, false));
            assert!(!default_route(ip, 1, u32::MAX, false));
            assert!(!default_route(ip, 0, 0, false));
            assert!(!default_route(ip, 0, u32::MAX, true));
        }
        assert!(!default_route("10.0.0.0".parse().unwrap(), 8, 100, false));
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "opt-in: requires an active physical LAN adapter; run with --ignored"]
    fn native_candidates_include_an_up_physical_address() -> anyhow::Result<()> {
        let raw = native::adapters()?;
        let routes = native::default_luids()?;
        eprintln!("Windows adapters: {raw:#?}\nDefault route LUIDs: {routes:?}");
        let result = candidates()?;
        eprintln!("Windows candidates: {result:#?}");
        assert!(
            result
                .iter()
                .any(|i| i.up && i.physical && !i.ip.is_loopback())
        );
        Ok(())
    }
}
