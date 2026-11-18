//! Port of `pkg/registry/core/service/ipallocator` (IPv4, bitmap-based).
//!
//! The network and broadcast addresses of the service CIDR are never handed out.

use std::collections::BTreeSet;
use std::net::Ipv4Addr;

pub struct IpAllocator {
    cidr: String,
    base: u32,
    size: u32,
    used: BTreeSet<u32>,
    next: u32,
}

impl IpAllocator {
    /// `cidr` like `10.96.0.0/12`.
    pub fn new(cidr: &str) -> Result<Self, String> {
        let (ip, bits) = cidr.split_once('/').ok_or_else(|| format!("invalid CIDR {cidr:?}"))?;
        let ip: Ipv4Addr = ip.parse().map_err(|_| format!("invalid CIDR {cidr:?}"))?;
        let bits: u32 = bits.parse().map_err(|_| format!("invalid CIDR {cidr:?}"))?;
        if !(8..=30).contains(&bits) {
            return Err(format!("service CIDR {cidr:?} must have a prefix between /8 and /30"));
        }
        let mask = u32::MAX << (32 - bits);
        let base = u32::from(ip) & mask;
        let size = 1u32 << (32 - bits);
        Ok(Self { cidr: cidr.into(), base, size, used: BTreeSet::new(), next: 1 })
    }

    pub fn cidr(&self) -> &str {
        &self.cidr
    }

    /// The first usable address (`10.96.0.1`), reserved for the `kubernetes` service.
    pub fn first(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.base + 1)
    }

    fn offset(&self, ip: &str) -> Result<u32, String> {
        let addr: Ipv4Addr = ip.parse().map_err(|_| format!("{ip} is not a valid IP address"))?;
        let v = u32::from(addr);
        if v < self.base || v >= self.base + self.size {
            return Err(format!("the provided IP ({ip}) is not in the valid range. The range of valid IPs is {}", self.cidr));
        }
        let off = v - self.base;
        if off == 0 || off == self.size - 1 {
            return Err(format!("the provided IP ({ip}) is not in the valid range. The range of valid IPs is {}", self.cidr));
        }
        Ok(off)
    }

    /// Reserves a specific IP.
    pub fn allocate(&mut self, ip: &str) -> Result<(), String> {
        let off = self.offset(ip)?;
        if !self.used.insert(off) {
            return Err(format!("the provided IP ({ip}) is already allocated"));
        }
        Ok(())
    }

    /// Allocates the next free IP (round-robin).
    pub fn allocate_next(&mut self) -> Result<String, String> {
        let usable = self.size - 2;
        for i in 0..usable {
            let off = 1 + (self.next - 1 + i) % usable;
            if self.used.insert(off) {
                self.next = off % usable + 1;
                return Ok(Ipv4Addr::from(self.base + off).to_string());
            }
        }
        Err(format!("range is full: no IP addresses left in {}", self.cidr))
    }

    pub fn release(&mut self, ip: &str) {
        if let Ok(off) = self.offset(ip) {
            self.used.remove(&off);
        }
    }

    pub fn used(&self) -> usize {
        self.used.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocates() {
        let mut a = IpAllocator::new("10.96.0.0/29").unwrap(); // 6 usable
        assert_eq!(a.first().to_string(), "10.96.0.1");
        a.allocate("10.96.0.1").unwrap();
        assert!(a.allocate("10.96.0.1").is_err());
        assert!(a.allocate("10.96.0.0").is_err());
        assert!(a.allocate("10.96.0.7").is_err());
        assert!(a.allocate("10.97.0.1").is_err());
        let mut got = vec![];
        for _ in 0..5 {
            got.push(a.allocate_next().unwrap());
        }
        assert!(!got.contains(&"10.96.0.1".to_string()));
        assert!(a.allocate_next().is_err());
        a.release("10.96.0.3");
        assert_eq!(a.allocate_next().unwrap(), "10.96.0.3");
    }
}
