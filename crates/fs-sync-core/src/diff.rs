//! 差异计算：本地索引 vs 远端索引 → 同步动作列表（纯逻辑，提示词 4.4 规则）。

use crate::entry::FileEntry;
use crate::version::{self, VersionVector};
use std::collections::HashMap;

/// 同步动作：由 diff 计算得出，由 daemon 执行。
#[allow(clippy::large_enum_variant)] // Conflict 携带双方条目本体，调用频率低，装箱得不偿失
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// 本地是胜者：把该条目推给对端（内容按需传输）
    Push { path: String },
    /// 对端是胜者：从对端拉取内容
    Pull { path: String },
    /// 内容一致但版本向量不同：本地合并版本向量，无需传内容
    MergeMeta { path: String, merged: VersionVector },
    /// 冲突：胜者内容落原路径，败者内容另存冲突副本
    /// `winner_side` 指明哪边内容保留在原路径
    Conflict {
        path: String,
        winner_side: Side,
        winner: FileEntry,
        loser: FileEntry,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Local,
    Remote,
}

/// 冲突胜者判定：mtime 较新者胜；相同则设备 ID 字典序大者胜（提示词 4.4）。
pub fn conflict_winner(local: &FileEntry, remote: &FileEntry) -> Side {
    use Side::*;
    match local.mtime.cmp(&remote.mtime) {
        std::cmp::Ordering::Greater => Local,
        std::cmp::Ordering::Less => Remote,
        std::cmp::Ordering::Equal => {
            if local.modified_by >= remote.modified_by {
                Local
            } else {
                Remote
            }
        }
    }
}

/// 计算 本地索引 → 需要执行的同步动作。
///
/// `local`/`remote`：path → entry（含墓碑）。返回动作按 path 排序，保证确定性。
pub fn diff(
    local: &HashMap<String, FileEntry>,
    remote: &HashMap<String, FileEntry>,
) -> Vec<SyncAction> {
    let mut actions = Vec::new();
    let mut paths: Vec<&String> = local.keys().chain(remote.keys()).collect();
    paths.sort();
    paths.dedup();

    for path in paths {
        let l = local.get(path);
        let r = remote.get(path);

        match (l, r) {
            // 本地独有：远端被删过（墓碑）则本地也删（远端版本支配）；否则是新增 → 推送
            (Some(l), None) => {
                if l.deleted {
                    // 本地墓碑、远端无记录：无需动作
                } else {
                    actions.push(SyncAction::Push { path: path.clone() });
                }
            }
            // 远端独有：本地被删过（墓碑）→ 远端是删除后重来的？不：远端有而本地墓碑，
            // 说明本地删除早于远端此条目产生，远端支配 → 拉取（修改优先于删除在这里体现为远端有新版本）
            (None, Some(r)) => {
                if r.deleted {
                    // 远端墓碑、本地无记录：一致（都视为不存在）
                } else {
                    actions.push(SyncAction::Pull { path: path.clone() });
                }
            }
            (Some(l), Some(r)) => {
                match version::compare(&l.version, &r.version) {
                    version::Ordering::Equal => {
                        if l.hash == r.hash {
                            // 完全一致
                        } else if l.deleted == r.deleted {
                            // 同版本向量不同内容：异常状态，保守按冲突
                            let side = conflict_winner(l, r);
                            actions.push(SyncAction::Conflict {
                                path: path.clone(),
                                winner_side: side,
                                winner: pick(l, r, side).clone(),
                                loser: pick(l, r, flip(side)).clone(),
                            });
                        }
                    }
                    version::Ordering::Dominates => {
                        if l.hash != r.hash || l.deleted != r.deleted {
                            actions.push(SyncAction::Push { path: path.clone() });
                        }
                    }
                    version::Ordering::DominatedBy => {
                        if l.hash != r.hash || l.deleted != r.deleted {
                            actions.push(SyncAction::Pull { path: path.clone() });
                        }
                    }
                    version::Ordering::Concurrent => {
                        if l.hash == r.hash && l.deleted == r.deleted {
                            // 内容一致，仅合并版本向量
                            actions.push(SyncAction::MergeMeta {
                                path: path.clone(),
                                merged: version::merge(&l.version, &r.version),
                            });
                        } else {
                            let side = conflict_winner(l, r);
                            actions.push(SyncAction::Conflict {
                                path: path.clone(),
                                winner_side: side,
                                winner: pick(l, r, side).clone(),
                                loser: pick(l, r, flip(side)).clone(),
                            });
                        }
                    }
                }
            }
            (None, None) => unreachable!(),
        }
    }
    actions
}

fn pick<'a>(l: &'a FileEntry, r: &'a FileEntry, side: Side) -> &'a FileEntry {
    match side {
        Side::Local => l,
        Side::Remote => r,
    }
}

fn flip(s: Side) -> Side {
    match s {
        Side::Local => Side::Remote,
        Side::Remote => Side::Local,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::version::VersionVector;
    use std::collections::HashMap;

    fn entry(path: &str, hash: &str, mtime: i64, dev: &str, counter: u64) -> FileEntry {
        let mut e = FileEntry::new(path, 10, mtime, hash, false, dev);
        // 重置版本向量到指定 counter，便于构造场景
        e.version = VersionVector::new();
        for _ in 0..counter {
            e.version.increment(dev);
        }
        e
    }

    fn map(entries: Vec<FileEntry>) -> HashMap<String, FileEntry> {
        entries.into_iter().map(|e| (e.path.clone(), e)).collect()
    }

    #[test]
    fn local_new_pushes() {
        let l = map(vec![entry("a.txt", "h1", 100, "A", 1)]);
        let r = map(vec![]);
        assert_eq!(
            diff(&l, &r),
            vec![SyncAction::Push {
                path: "a.txt".into()
            }]
        );
    }

    #[test]
    fn remote_new_pulls() {
        let l = map(vec![]);
        let r = map(vec![entry("b.txt", "h1", 100, "B", 1)]);
        assert_eq!(
            diff(&l, &r),
            vec![SyncAction::Pull {
                path: "b.txt".into()
            }]
        );
    }

    #[test]
    fn local_tombstone_no_record_noop() {
        let mut t = entry("a.txt", "h1", 100, "A", 2);
        t.deleted = true;
        let l = map(vec![t]);
        let r = map(vec![]);
        assert!(diff(&l, &r).is_empty());
    }

    #[test]
    fn local_dominates_pushes() {
        let l = map(vec![entry("a.txt", "h2", 200, "A", 3)]);
        let r = map(vec![entry("a.txt", "h1", 100, "A", 2)]);
        assert_eq!(
            diff(&l, &r),
            vec![SyncAction::Push {
                path: "a.txt".into()
            }]
        );
    }

    #[test]
    fn remote_dominates_pulls() {
        let l = map(vec![entry("a.txt", "h1", 100, "A", 1)]);
        // 让远端版本严格支配本地：{A:2, B:1}
        let mut re = entry("a.txt", "h2", 200, "A", 2);
        re.version.increment("B");
        let r = map(vec![re]);
        assert_eq!(
            diff(&l, &r),
            vec![SyncAction::Pull {
                path: "a.txt".into()
            }]
        );
    }

    #[test]
    fn concurrent_same_hash_merges_meta() {
        let l = map(vec![entry("a.txt", "h1", 100, "A", 1)]);
        let r = map(vec![entry("a.txt", "h1", 100, "B", 1)]);
        let actions = diff(&l, &r);
        match &actions[0] {
            SyncAction::MergeMeta { merged, .. } => {
                assert_eq!(merged.get("A"), 1);
                assert_eq!(merged.get("B"), 1);
            }
            other => panic!("期望 MergeMeta，实际 {other:?}"),
        }
    }

    #[test]
    fn concurrent_different_hash_newer_mtime_wins() {
        let l = map(vec![entry("a.txt", "h_local", 300, "A", 1)]);
        let r = map(vec![entry("a.txt", "h_remote", 200, "B", 1)]);
        let actions = diff(&l, &r);
        match &actions[0] {
            SyncAction::Conflict {
                winner_side,
                winner,
                loser,
                ..
            } => {
                assert_eq!(*winner_side, Side::Local);
                assert_eq!(winner.hash, "h_local");
                assert_eq!(loser.hash, "h_remote");
            }
            other => panic!("期望 Conflict，实际 {other:?}"),
        }
    }

    #[test]
    fn concurrent_same_mtime_device_id_breaks_tie() {
        // modified_by "B" > "A"，本地 modified_by 是 "B" → 本地胜
        let l = map(vec![entry("a.txt", "h_local", 100, "B", 1)]);
        let r = map(vec![entry("a.txt", "h_remote", 100, "A", 1)]);
        let actions = diff(&l, &r);
        match &actions[0] {
            SyncAction::Conflict { winner_side, .. } => assert_eq!(*winner_side, Side::Local),
            other => panic!("期望 Conflict，实际 {other:?}"),
        }
    }

    #[test]
    fn delete_vs_concurrent_modify_recovers_file() {
        // 本地删除（墓碑）、远端并发修改 → 冲突且按规则远端修改较新则恢复
        let mut lt = entry("a.txt", "h1", 100, "A", 1);
        lt.deleted = true;
        let l = map(vec![lt]);
        let r = map(vec![entry("a.txt", "h2", 500, "B", 2)]);
        let actions = diff(&l, &r);
        match &actions[0] {
            SyncAction::Conflict {
                winner_side,
                winner,
                ..
            } => {
                assert_eq!(*winner_side, Side::Remote);
                assert_eq!(winner.hash, "h2");
            }
            other => panic!("期望 Conflict，实际 {other:?}"),
        }
    }
}
