use super::*;

// macOS en<N> covers built-in Wi-Fi/Ethernet and adapters. Bridges, awdl/llw
// (peer-to-peer Wi-Fi), vmnet, and arbitrary utun VPNs are not physical hints.
// Tailscale's 100.64/10 utun address is explicitly eligible in prioritize().
fn physical(name: &str) -> bool {
    name.strip_prefix("en")
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
}
pub(crate) fn candidates() -> Result<Vec<InterfaceAddress>> {
    let interfaces = if_addrs::get_if_addrs()?
        .into_iter()
        .map(|i| InterfaceAddress {
            ip: i.ip(),
            physical: physical(&i.name),
            up: i.is_oper_up(),
            // Query the per-address Darwin flag, not the interface's flags.
            // Failure is conservative: do not advertise potentially temporary IPv6.
            temporary: match i.ip() {
                IpAddr::V6(ip) => temporary(&i.name, ip).unwrap_or(true),
                _ => false,
            },
            name: i.name,
        })
        .filter(|i| !i.temporary)
        .collect();
    // No /proc route tables on Darwin. Physical LAN addresses and Tailscale
    // suffice; route metric selection can be added without affecting transport.
    Ok(prioritize(interfaces, &HashSet::new()))
}
fn temporary(name: &str, ip: std::net::Ipv6Addr) -> Option<bool> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    // SDK netinet6/in6_var.h: char[16] + 272-byte union (8-byte aligned),
    // size 288. SIOCGIFAFLAG_IN6 = _IOWR('i',73,struct in6_ifreq).
    #[repr(C, align(8))]
    struct Request {
        name: [u8; 16],
        value: [u8; 272],
    }
    if name.len() >= 16 {
        return None;
    }
    let mut request = Request {
        name: [0; 16],
        value: [0; 272],
    };
    request.name[..name.len()].copy_from_slice(name.as_bytes());
    let address = libc::sockaddr_in6 {
        sin6_len: std::mem::size_of::<libc::sockaddr_in6>() as u8,
        sin6_family: libc::AF_INET6 as u8,
        sin6_port: 0,
        sin6_flowinfo: 0,
        sin6_addr: libc::in6_addr {
            s6_addr: ip.octets(),
        },
        sin6_scope_id: 0,
    };
    // A byte copy avoids imposing sockaddr alignment on the ioctl union buffer.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&address as *const libc::sockaddr_in6).cast::<u8>(),
            request.value.as_mut_ptr(),
            std::mem::size_of_val(&address),
        );
    }
    let fd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM, 0) };
    if fd < 0 {
        return None;
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let result = unsafe { libc::ioctl(fd.as_raw_fd(), 0xc1206949 as libc::c_ulong, &mut request) };
    (result == 0)
        .then(|| i32::from_ne_bytes(request.value[..4].try_into().expect("flags")) & 0x0080 != 0)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_names_and_utun_tailscale_are_eligible() {
        assert!(physical("en0"));
        assert!(physical("en12"));
        for name in ["en", "en0x", "awdl0", "llw0", "utun4", "bridge0"] {
            assert!(!physical(name));
        }
        let hints = prioritize(
            vec![InterfaceAddress {
                name: "utun9".into(),
                ip: "100.82.21.53".parse().unwrap(),
                up: true,
                physical: false,
                temporary: false,
            }],
            &HashSet::new(),
        );
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].name, "utun9");
    }
}
