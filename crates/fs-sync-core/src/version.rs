//! 版本向量：device_id -> 单调递增计数器。
//!
//! 比较规则（与提示词 4.4 一致）：
//! - A 支配 B：A 的每个分量 >= B 的对应分量且至少一个严格大于 → A 胜出
//! - 互不支配 → 并发（潜在冲突）

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 版本向量。BTreeMap 使序列化字节序稳定，便于哈希与比较。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionVector(pub BTreeMap<String, u64>);

impl VersionVector {
    pub fn new() -> Self {
        Self(BTreeMap::new())
    }

    /// 指定设备计数器 +1，返回新值。
    pub fn increment(&mut self, device: &str) -> u64 {
        let c = self.0.entry(device.to_string()).or_insert(0);
        *c += 1;
        *c
    }

    pub fn get(&self, device: &str) -> u64 {
        self.0.get(device).copied().unwrap_or(0)
    }

    /// 合并两个版本向量：逐分量取 max（不产生新事件，仅收敛认知）。
    pub fn merged(&self, other: &VersionVector) -> VersionVector {
        let mut out = self.0.clone();
        for (d, c) in &other.0 {
            let e = out.entry(d.clone()).or_insert(0);
            *e = (*e).max(*c);
        }
        VersionVector(out)
    }
}

/// 版本向量比较结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ordering {
    /// self 支配 other（self 更新）
    Dominates,
    /// other 支配 self
    DominatedBy,
    /// 相等
    Equal,
    /// 并发（互不支配）
    Concurrent,
}

/// 比较两个版本向量。
pub fn compare(a: &VersionVector, b: &VersionVector) -> Ordering {
    if a.0 == b.0 {
        return Ordering::Equal;
    }
    let mut a_ge = true;
    let mut b_ge = true;
    let devices = a.0.keys().chain(b.0.keys()).collect::<Vec<_>>();
    for d in devices {
        let (x, y) = (a.get(d), b.get(d));
        if x < y {
            a_ge = false;
        }
        if y < x {
            b_ge = false;
        }
    }
    match (a_ge, b_ge) {
        (true, _) => Ordering::Dominates,
        (_, true) => Ordering::DominatedBy,
        (false, false) => Ordering::Concurrent,
    }
}

/// 合并（取逐分量 max）。
pub fn merge(a: &VersionVector, b: &VersionVector) -> VersionVector {
    a.merged(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vv(pairs: &[(&str, u64)]) -> VersionVector {
        let mut v = VersionVector::new();
        for (d, c) in pairs {
            v.0.insert(d.to_string(), *c);
        }
        v
    }

    #[test]
    fn dominates_when_all_components_ge() {
        let a = vv(&[("A", 3), ("B", 1)]);
        let b = vv(&[("A", 2), ("B", 1)]);
        assert_eq!(compare(&a, &b), Ordering::Dominates);
        assert_eq!(compare(&b, &a), Ordering::DominatedBy);
    }

    #[test]
    fn concurrent_when_both_diverge() {
        let a = vv(&[("A", 2)]);
        let b = vv(&[("B", 1)]);
        assert_eq!(compare(&a, &b), Ordering::Concurrent);
    }

    #[test]
    fn equal_vectors() {
        let a = vv(&[("A", 1)]);
        let b = vv(&[("A", 1)]);
        assert_eq!(compare(&a, &b), Ordering::Equal);
    }

    #[test]
    fn missing_device_counts_as_zero() {
        let a = vv(&[("A", 1), ("B", 1)]);
        let b = vv(&[("A", 1)]);
        assert_eq!(compare(&a, &b), Ordering::Dominates);
    }

    #[test]
    fn merge_takes_component_max() {
        let a = vv(&[("A", 2), ("B", 1)]);
        let b = vv(&[("A", 1), ("C", 5)]);
        let m = merge(&a, &b);
        assert_eq!(m.get("A"), 2);
        assert_eq!(m.get("B"), 1);
        assert_eq!(m.get("C"), 5);
    }

    #[test]
    fn dominates_single_component() {
        let mut a = VersionVector::default();
        a.increment("B");
        let mut b = a.clone();
        b.increment("B");
        assert_eq!(
            compare(&a, &b),
            Ordering::DominatedBy,
            "{{B:1}} 应被 {{B:2}} 支配"
        );
        assert_eq!(compare(&b, &a), Ordering::Dominates);
    }
}
