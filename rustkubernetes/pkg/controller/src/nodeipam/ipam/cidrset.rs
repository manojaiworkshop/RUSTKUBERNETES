//! Port of `pkg/controller/nodeipam/ipam/cidrset/cidr_set.go`: splits a
//! cluster CIDR into node-sized subnets and tracks which are in use.

use std::collections::HashSet;
use std::fmt;

use k8s_apimachinery::net::IpNet;

/// The largest supported difference between cluster and node prefix length
/// for IPv6 (`clusterSubnetMaxDiff`).
const CLUSTER_SUBNET_MAX_DIFF: u8 = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CidrSetError {
    /// `ErrCIDRRangeNoCIDRsRemaining`
    NoCidrsRemaining,
    /// `ErrCIDRSetSubNetTooBig`
    SubNetTooBig,
    Other(String),
}

impl fmt::Display for CidrSetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCidrsRemaining => {
                f.write_str("CIDR allocation failed; there are no remaining CIDRs left to allocate in the accepted range")
            }
            Self::SubNetTooBig => f.write_str("New CIDR set failed; the node CIDR size is too big"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for CidrSetError {}

/// `CidrSet`
#[derive(Debug)]
pub struct CidrSet {
    cluster_cidr: IpNet,
    node_mask_size: u8,
    max_cidrs: u64,
    next_candidate: u64,
    used: HashSet<u64>,
}

impl CidrSet {
    /// `NewCIDRSet`
    pub fn new(cluster_cidr: IpNet, node_mask_size: u8) -> Result<Self, CidrSetError> {
        let cluster_mask = cluster_cidr.prefix;
        if node_mask_size < cluster_mask || node_mask_size > cluster_cidr.bits() {
            return Err(CidrSetError::Other(format!("invalid node CIDR mask size {node_mask_size} for cluster CIDR {cluster_cidr}")));
        }
        if !cluster_cidr.is_ipv4() && node_mask_size - cluster_mask > CLUSTER_SUBNET_MAX_DIFF {
            return Err(CidrSetError::SubNetTooBig);
        }
        Ok(Self {
            cluster_cidr,
            node_mask_size,
            max_cidrs: 1u64 << (node_mask_size - cluster_mask),
            next_candidate: 0,
            used: HashSet::new(),
        })
    }

    pub fn max_cidrs(&self) -> u64 {
        self.max_cidrs
    }

    pub fn allocated(&self) -> usize {
        self.used.len()
    }

    fn host_bits(&self) -> u32 {
        (self.cluster_cidr.bits() - self.node_mask_size) as u32
    }

    /// `indexToCIDRBlock`
    fn index_to_cidr(&self, index: u64) -> IpNet {
        let offset = if self.host_bits() >= 128 { 0 } else { (index as u128) << self.host_bits() };
        IpNet { addr: self.cluster_cidr.nth(offset), prefix: self.node_mask_size }
    }

    /// `AllocateNext`: the next free subnet, starting after the last allocation.
    pub fn allocate_next(&mut self) -> Result<IpNet, CidrSetError> {
        if self.used.len() as u64 >= self.max_cidrs {
            return Err(CidrSetError::NoCidrsRemaining);
        }
        let mut candidate = self.next_candidate;
        for _ in 0..self.max_cidrs {
            if !self.used.contains(&candidate) {
                break;
            }
            candidate = (candidate + 1) % self.max_cidrs;
        }
        self.next_candidate = (candidate + 1) % self.max_cidrs;
        self.used.insert(candidate);
        Ok(self.index_to_cidr(candidate))
    }

    /// `getIndexForIP`
    fn index_for_ip(&self, ip: u128) -> Result<u64, CidrSetError> {
        let idx = (ip - self.cluster_cidr.base()).checked_shr(self.host_bits()).unwrap_or(0);
        if idx >= self.max_cidrs as u128 {
            return Err(CidrSetError::Other(format!(
                "CIDR: {}/{} is out of the range of CIDR allocator",
                k8s_apimachinery::net::u128_to_ip(ip, self.cluster_cidr.is_ipv4()),
                self.node_mask_size
            )));
        }
        Ok(idx as u64)
    }

    /// `getBeginningAndEndIndices`: the node subnets a CIDR covers. A CIDR
    /// larger than the cluster CIDR covers all of them.
    fn indices(&self, cidr: &IpNet) -> Result<(u64, u64), CidrSetError> {
        if !self.cluster_cidr.overlaps(cidr) {
            return Err(CidrSetError::Other(format!("cidr {cidr} is out the range of cluster cidr {}", self.cluster_cidr)));
        }
        let (mut begin, mut end) = (0, self.max_cidrs - 1);
        if self.cluster_cidr.prefix < cidr.prefix {
            let node_mask = |v: u128| {
                let hb = self.host_bits();
                if hb >= 128 {
                    0
                } else {
                    v >> hb << hb
                }
            };
            begin = self.index_for_ip(node_mask(cidr.base()))?;
            end = self.index_for_ip(node_mask(cidr.last()))?;
        }
        Ok((begin, end))
    }

    /// `Occupy`: marks every node subnet overlapping `cidr` as used.
    pub fn occupy(&mut self, cidr: &IpNet) -> Result<(), CidrSetError> {
        let (b, e) = self.indices(cidr)?;
        for i in b..=e {
            self.used.insert(i);
        }
        Ok(())
    }

    /// `Release`
    pub fn release(&mut self, cidr: &IpNet) -> Result<(), CidrSetError> {
        let (b, e) = self.indices(cidr)?;
        for i in b..=e {
            self.used.remove(&i);
        }
        Ok(())
    }

    pub fn is_used(&self, cidr: &IpNet) -> bool {
        self.indices(cidr).map(|(b, e)| (b..=e).all(|i| self.used.contains(&i))).unwrap_or(false)
    }

    pub fn cluster_cidr(&self) -> &IpNet {
        &self.cluster_cidr
    }

    #[cfg(test)]
    fn contains_ip(&self, ip: std::net::IpAddr) -> bool {
        self.cluster_cidr.contains(ip)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn net(s: &str) -> IpNet {
        s.parse().unwrap()
    }

    // TestCIDRSetFullyAllocated
    #[test]
    fn fully_allocated() {
        let mut s = CidrSet::new(net("127.123.234.0/30"), 30).unwrap();
        assert_eq!(s.allocate_next().unwrap().to_string(), "127.123.234.0/30");
        assert_eq!(s.allocate_next(), Err(CidrSetError::NoCidrsRemaining));
        s.release(&net("127.123.234.0/30")).unwrap();
        assert_eq!(s.allocate_next().unwrap().to_string(), "127.123.234.0/30");
        assert!(s.allocate_next().is_err());
    }

    // TestIndexToCIDRBlock / AllocateNext order
    #[test]
    fn allocates_in_order_and_wraps() {
        let mut s = CidrSet::new(net("10.244.0.0/16"), 24).unwrap();
        assert_eq!(s.max_cidrs(), 256);
        for i in 0..3 {
            assert_eq!(s.allocate_next().unwrap().to_string(), format!("10.244.{i}.0/24"));
        }
        s.release(&net("10.244.0.0/24")).unwrap();
        // next candidate continues after the last allocation, then wraps around
        assert_eq!(s.allocate_next().unwrap().to_string(), "10.244.3.0/24");
        let v6 = CidrSet::new(net("fd00:10:244::/56"), 64).unwrap();
        assert_eq!(v6.index_to_cidr(1).to_string(), "fd00:10:244:1::/64");
        assert!(v6.contains_ip("fd00:10:244:1::1".parse().unwrap()));
    }

    // TestOccupy
    #[test]
    fn occupy_ranges() {
        let mut s = CidrSet::new(net("10.0.0.0/8"), 16).unwrap();
        s.occupy(&net("10.0.0.0/14")).unwrap(); // 4 node subnets
        assert_eq!(s.allocated(), 4);
        assert_eq!(s.allocate_next().unwrap().to_string(), "10.4.0.0/16");
        s.occupy(&net("10.200.3.0/24")).unwrap(); // smaller than a node subnet: occupies its parent
        assert!(s.is_used(&net("10.200.0.0/16")));
        let mut whole = CidrSet::new(net("10.244.0.0/16"), 24).unwrap();
        whole.occupy(&net("10.0.0.0/8")).unwrap(); // larger than the cluster CIDR: everything
        assert_eq!(whole.allocate_next(), Err(CidrSetError::NoCidrsRemaining));
        assert!(s.occupy(&net("192.168.0.0/16")).is_err());
    }

    // TestCIDRSetSubNetTooBig
    #[test]
    fn subnet_limits() {
        assert_eq!(CidrSet::new(net("fd00::/40"), 64).unwrap_err(), CidrSetError::SubNetTooBig);
        assert!(CidrSet::new(net("10.0.0.0/8"), 32).is_ok());
        assert!(CidrSet::new(net("10.244.0.0/16"), 8).is_err());
    }
}
