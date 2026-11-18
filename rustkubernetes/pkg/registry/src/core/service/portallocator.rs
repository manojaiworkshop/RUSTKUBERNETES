//! Port of `pkg/registry/core/service/portallocator` (NodePort range, default 30000-32767).

use std::collections::BTreeSet;

pub struct PortAllocator {
    min: i64,
    max: i64,
    used: BTreeSet<i64>,
    next: i64,
}

impl PortAllocator {
    /// `range` like `30000-32767`.
    pub fn new(range: &str) -> Result<Self, String> {
        let (a, b) = range.split_once('-').ok_or_else(|| format!("invalid port range {range:?}"))?;
        let min: i64 = a.trim().parse().map_err(|_| format!("invalid port range {range:?}"))?;
        let max: i64 = b.trim().parse().map_err(|_| format!("invalid port range {range:?}"))?;
        if min < 1 || max > 65535 || min > max {
            return Err(format!("invalid port range {range:?}"));
        }
        Ok(Self { min, max, used: BTreeSet::new(), next: min })
    }

    pub fn range(&self) -> String {
        format!("{}-{}", self.min, self.max)
    }

    pub fn allocate(&mut self, port: i64) -> Result<(), String> {
        if port < self.min || port > self.max {
            return Err(format!("provided port is not in the valid range. The range of valid ports is {}", self.range()));
        }
        if !self.used.insert(port) {
            return Err("provided port is already allocated".into());
        }
        Ok(())
    }

    pub fn allocate_next(&mut self) -> Result<i64, String> {
        let span = self.max - self.min + 1;
        for i in 0..span {
            let p = self.min + (self.next - self.min + i) % span;
            if self.used.insert(p) {
                self.next = if p == self.max { self.min } else { p + 1 };
                return Ok(p);
            }
        }
        Err(format!("range is full: no ports left in {}", self.range()))
    }

    pub fn release(&mut self, port: i64) {
        self.used.remove(&port);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ports() {
        let mut p = PortAllocator::new("30000-30002").unwrap();
        p.allocate(30001).unwrap();
        assert!(p.allocate(30001).is_err());
        assert!(p.allocate(80).is_err());
        assert_eq!(p.allocate_next().unwrap(), 30000);
        assert_eq!(p.allocate_next().unwrap(), 30002);
        assert!(p.allocate_next().is_err());
        p.release(30000);
        assert_eq!(p.allocate_next().unwrap(), 30000);
    }
}
