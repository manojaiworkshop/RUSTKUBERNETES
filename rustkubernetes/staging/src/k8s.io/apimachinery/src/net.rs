//! IP network helpers (`k8s.io/utils/net`: `ParseCIDRSloppy`, `IsIPv4CIDR`,
//! `AddIPOffset`, ...), shared by validation, the node IPAM controller and the
//! kubelet.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// A CIDR as Go's `net.ParseCIDR` returns it: the network address (host bits
/// cleared) and the prefix length.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IpNet {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl IpNet {
    pub fn is_ipv4(&self) -> bool {
        self.addr.is_ipv4()
    }

    /// 32 or 128.
    pub fn bits(&self) -> u8 {
        if self.is_ipv4() {
            32
        } else {
            128
        }
    }

    /// The network address as an integer.
    pub fn base(&self) -> u128 {
        ip_to_u128(self.addr)
    }

    /// Last address of the network as an integer.
    pub fn last(&self) -> u128 {
        let host_bits = (self.bits() - self.prefix) as u32;
        if host_bits == 0 {
            self.base()
        } else if host_bits >= 128 {
            u128::MAX
        } else {
            self.base() | ((1u128 << host_bits) - 1)
        }
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        ip.is_ipv4() == self.is_ipv4() && (self.base()..=self.last()).contains(&ip_to_u128(ip))
    }

    /// Whether the two networks share at least one address.
    pub fn overlaps(&self, other: &IpNet) -> bool {
        self.is_ipv4() == other.is_ipv4() && self.base() <= other.last() && other.base() <= self.last()
    }

    /// `base + n` in this network's family (`AddIPOffset`).
    pub fn nth(&self, n: u128) -> IpAddr {
        u128_to_ip(self.base().wrapping_add(n), self.is_ipv4())
    }
}

pub fn ip_to_u128(ip: IpAddr) -> u128 {
    match ip {
        IpAddr::V4(v) => u32::from(v) as u128,
        IpAddr::V6(v) => u128::from(v),
    }
}

pub fn u128_to_ip(n: u128, v4: bool) -> IpAddr {
    if v4 {
        IpAddr::V4(Ipv4Addr::from(n as u32))
    } else {
        IpAddr::V6(Ipv6Addr::from(n))
    }
}

/// The netmask with `prefix` leading ones, as an integer of `bits` width.
fn mask(prefix: u8, bits: u8) -> u128 {
    let width_mask = if bits >= 128 { u128::MAX } else { (1u128 << bits) - 1 };
    if prefix == 0 {
        0
    } else {
        (width_mask << (bits - prefix)) & width_mask
    }
}

impl FromStr for IpNet {
    type Err = String;

    /// `ParseCIDRSloppy`: `addr/prefix`, host bits are cleared.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let err = || format!("invalid CIDR address: {s}");
        let (a, p) = s.trim().split_once('/').ok_or_else(err)?;
        let addr: IpAddr = a.parse().map_err(|_| err())?;
        if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
            return Err(err());
        }
        let prefix: u8 = p.parse().map_err(|_| err())?;
        let bits = if addr.is_ipv4() { 32 } else { 128 };
        if prefix > bits {
            return Err(err());
        }
        let n = ip_to_u128(addr) & mask(prefix, bits);
        Ok(IpNet { addr: u128_to_ip(n, addr.is_ipv4()), prefix })
    }
}

impl fmt::Display for IpNet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

/// Parses a comma-separated list of CIDRs (`ParseCIDRs`).
pub fn parse_cidrs(s: &str) -> Result<Vec<IpNet>, String> {
    s.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::parse).collect()
}

/// `IsDualStackCIDRs`: exactly one IPv4 and one IPv6 network.
pub fn is_dual_stack(cidrs: &[IpNet]) -> bool {
    cidrs.len() == 2 && cidrs[0].is_ipv4() != cidrs[1].is_ipv4()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_masks() {
        let n: IpNet = "10.244.3.7/16".parse().unwrap();
        assert_eq!(n.to_string(), "10.244.0.0/16");
        assert!(n.contains("10.244.255.1".parse().unwrap()));
        assert!(!n.contains("10.245.0.1".parse().unwrap()));
        assert_eq!(n.nth(1).to_string(), "10.244.0.1");
        let v6: IpNet = "fd00:10:244::/56".parse().unwrap();
        assert_eq!(v6.bits(), 128);
        assert!(v6.contains("fd00:10:244:ff::1".parse().unwrap()));
        assert!("10.0.0.0/33".parse::<IpNet>().is_err());
        assert!("10.0.0.0".parse::<IpNet>().is_err());
        assert!("0.0.0.0/0".parse::<IpNet>().unwrap().contains("1.2.3.4".parse().unwrap()));
    }

    #[test]
    fn overlap() {
        let a: IpNet = "10.96.0.0/12".parse().unwrap();
        assert!(a.overlaps(&"10.100.0.0/16".parse().unwrap()));
        assert!(!a.overlaps(&"10.244.0.0/16".parse().unwrap()));
        assert!(is_dual_stack(&parse_cidrs("10.244.0.0/16,fd00::/48").unwrap()));
    }
}
