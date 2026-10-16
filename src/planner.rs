//! The cleaning policy: what to delete, given what is on disk and how much space is needed.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::inventory::{AGE_BUCKET, Cache, CacheKind, Inventory, Usage, Workspace};

#[derive(Debug)]
pub enum Sweep {
    /// Frequent and cheap: finish interrupted deletions and drop output bases of deleted
    /// workspaces.
    Orphans,
    /// Also apply the idle deadlines, and free space when the disk is nearly full.
    Full { pressure: Option<Pressure> },
}

/// The disk is nearly full: `bytes_to_free` must go, least recently used first.
#[derive(Debug)]
pub struct Pressure {
    pub bytes_to_free: u64,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    PurgeTrash(PathBuf),
    DeleteOutputBase {
        path: PathBuf,
        reason: Reason,
    },
    DeleteInstallBase {
        path: PathBuf,
        reason: Reason,
    },
    /// Evict the entries of `cache` last used before `older_than`.
    EvictEntries {
        cache: Cache,
        older_than: SystemTime,
        reason: Reason,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    WorkspaceDeleted(PathBuf),
    Idle(Duration),
    DiskPressure,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Reason::WorkspaceDeleted(path) => write!(f, "workspace {} is gone", path.display()),
            Reason::Idle(idle) => write!(f, "unused for {}", humantime::format_duration(*idle)),
            Reason::DiskPressure => f.write_str("disk is nearly full"),
        }
    }
}

pub fn plan(inventory: &Inventory, now: SystemTime, config: &Config, sweep: Sweep) -> Vec<Action> {
    let mut actions: Vec<Action> = inventory
        .trash
        .iter()
        .cloned()
        .map(Action::PurgeTrash)
        .collect();
    let idle_since = |time: SystemTime| now.duration_since(time).unwrap_or_default();

    // Output bases not deleted by an earlier rule.
    let mut kept = Vec::new();
    for base in &inventory.output_bases {
        match &base.workspace {
            Workspace::Missing(workspace) => actions.push(Action::DeleteOutputBase {
                path: base.path.clone(),
                reason: Reason::WorkspaceDeleted(workspace.clone()),
            }),
            _ => kept.push(base),
        }
    }
    let Sweep::Full { pressure } = sweep else {
        return actions;
    };

    for install in &inventory.install_bases {
        let idle = idle_since(install.last_used);
        if idle >= config.install_base_idle {
            actions.push(Action::DeleteInstallBase {
                path: install.path.clone(),
                reason: Reason::Idle(idle),
            });
        }
    }
    kept.retain(|base| {
        let idle = idle_since(base.last_used);
        let expired = idle >= config.output_base_idle;
        if expired {
            actions.push(Action::DeleteOutputBase {
                path: base.path.clone(),
                reason: Reason::Idle(idle),
            });
        }
        !expired
    });

    let expiry = now - config.cache_entry_idle;
    let mut cutoffs: HashMap<&Path, SystemTime> = HashMap::new();
    if let Some(pressure) = pressure {
        let usage = &pressure.usage;
        let floor = now - config.recent_use_floor;
        let mut freed: u64 = actions
            .iter()
            .map(|action| match action {
                Action::DeleteOutputBase { path, .. } => usage.size(path),
                _ => 0,
            })
            .sum();

        // Every candidate, keyed by the time it was last used.
        let mut queue: Vec<(SystemTime, Candidate)> = kept
            .iter()
            .filter(|base| base.last_used < floor)
            .map(|base| (base.last_used, Candidate::OutputBase(&base.path)))
            .collect();
        for cache in &inventory.caches {
            let Some(ages) = usage.ages.get(&cache.root) else {
                continue;
            };
            for (&start, &bytes) in ages {
                let end = start + AGE_BUCKET;
                if end <= expiry {
                    freed += bytes;
                } else if end <= floor {
                    queue.push((
                        start,
                        Candidate::Entries {
                            root: &cache.root,
                            end,
                            bytes,
                        },
                    ));
                }
            }
        }
        queue.sort_by_key(|(time, _)| *time);

        for (_, candidate) in queue {
            if freed >= pressure.bytes_to_free {
                break;
            }
            match candidate {
                Candidate::OutputBase(path) => {
                    freed += usage.size(path);
                    actions.push(Action::DeleteOutputBase {
                        path: path.to_path_buf(),
                        reason: Reason::DiskPressure,
                    });
                }
                Candidate::Entries { root, end, bytes } => {
                    freed += bytes;
                    cutoffs.insert(root, end);
                }
            }
        }
    }

    for cache in &inventory.caches {
        let action = match cutoffs.get(cache.root.as_path()) {
            Some(&cutoff) => Action::EvictEntries {
                cache: cache.clone(),
                older_than: cutoff,
                reason: Reason::DiskPressure,
            },
            _ => Action::EvictEntries {
                cache: cache.clone(),
                older_than: expiry,
                reason: Reason::Idle(config.cache_entry_idle),
            },
        };
        actions.push(action);
    }
    actions
}

enum Candidate<'a> {
    OutputBase(&'a Path),
    /// The disk-cache entries in the age bucket that ends at `end`.
    Entries {
        root: &'a Path,
        end: SystemTime,
        bytes: u64,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::{InstallBase, OutputBase, bucket_start};
    use std::collections::BTreeMap;

    const HOUR: Duration = Duration::from_secs(3600);

    fn now() -> SystemTime {
        bucket_start(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000))
    }

    fn base(name: &str, workspace: Workspace, idle: Duration) -> OutputBase {
        OutputBase {
            path: PathBuf::from(format!("/root/{name}")),
            workspace,
            last_used: now() - idle,
        }
    }

    fn present() -> Workspace {
        Workspace::Present("/work/repo".into())
    }

    fn disk_cache() -> Cache {
        Cache {
            root: "/cache/disk".into(),
            kind: CacheKind::Disk,
        }
    }

    fn deleted_bases(actions: &[Action]) -> Vec<(&str, &Reason)> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::DeleteOutputBase { path, reason } => Some((path.to_str()?, reason)),
                _ => None,
            })
            .collect()
    }

    fn eviction(actions: &[Action]) -> (SystemTime, &Reason) {
        actions
            .iter()
            .find_map(|action| match action {
                Action::EvictEntries {
                    older_than, reason, ..
                } => Some((*older_than, reason)),
                _ => None,
            })
            .unwrap()
    }

    #[test]
    fn orphans_go_on_every_sweep_but_unreachable_workspaces_stay() {
        let inventory = Inventory {
            output_bases: vec![
                base("gone", Workspace::Missing("/w/gone".into()), Duration::ZERO),
                base(
                    "volume",
                    Workspace::Unreachable("/Volumes/x/w".into()),
                    Duration::ZERO,
                ),
                base("live", present(), Duration::ZERO),
            ],
            trash: vec!["/root/.cendrillon-trash-old".into()],
            ..Inventory::default()
        };
        let actions = plan(&inventory, now(), &Config::default(), Sweep::Orphans);
        assert_eq!(
            actions,
            [
                Action::PurgeTrash("/root/.cendrillon-trash-old".into()),
                Action::DeleteOutputBase {
                    path: "/root/gone".into(),
                    reason: Reason::WorkspaceDeleted("/w/gone".into()),
                },
            ]
        );
    }

    #[test]
    fn idle_deadlines_apply_only_on_full_sweeps() {
        let inventory = Inventory {
            output_bases: vec![
                base("stale", present(), 25 * HOUR),
                base("recent", present(), 23 * HOUR),
            ],
            install_bases: vec![InstallBase {
                path: "/root/install/old".into(),
                last_used: now() - 73 * HOUR,
            }],
            caches: vec![disk_cache()],
            ..Inventory::default()
        };
        let config = Config::default();
        assert!(deleted_bases(&plan(&inventory, now(), &config, Sweep::Orphans)).is_empty());

        let actions = plan(&inventory, now(), &config, Sweep::Full { pressure: None });
        assert_eq!(
            deleted_bases(&actions),
            [("/root/stale", &Reason::Idle(25 * HOUR))]
        );
        assert!(actions.contains(&Action::DeleteInstallBase {
            path: "/root/install/old".into(),
            reason: Reason::Idle(73 * HOUR),
        }));
        assert_eq!(
            eviction(&actions),
            (now() - 72 * HOUR, &Reason::Idle(72 * HOUR))
        );
    }

    #[test]
    fn pressure_evicts_least_recently_used_first_until_enough_is_freed() {
        let cache = disk_cache();
        let inventory = Inventory {
            output_bases: vec![
                base("orphan", Workspace::Missing("/w/x".into()), HOUR),
                base("ten_hours", present(), 10 * HOUR),
                base("five_hours", present(), 5 * HOUR),
                base("one_hour", present(), HOUR),
            ],
            caches: vec![cache.clone()],
            ..Inventory::default()
        };
        let usage = Usage {
            sizes: HashMap::from([
                ("/root/orphan".into(), 100),
                ("/root/ten_hours".into(), 1000),
                ("/root/five_hours".into(), 1000),
                ("/root/one_hour".into(), 1000),
            ]),
            ages: HashMap::from([(
                cache.root.clone(),
                BTreeMap::from([
                    (now() - 100 * HOUR, 50), // already past the idle deadline
                    (now() - 20 * HOUR, 400),
                    (now() - 8 * HOUR, 400),
                    (now() - 3 * HOUR, 400),
                ]),
            )]),
        };
        let pressure = Pressure {
            // orphan + expired entries (150) + 20h and 10h-old items (1400) fall short by 50;
            // the 8h-old entries cover it.
            bytes_to_free: 1600,
            usage,
        };
        let actions = plan(
            &inventory,
            now(),
            &Config::default(),
            Sweep::Full {
                pressure: Some(pressure),
            },
        );
        assert_eq!(
            deleted_bases(&actions),
            [
                ("/root/orphan", &Reason::WorkspaceDeleted("/w/x".into())),
                ("/root/ten_hours", &Reason::DiskPressure),
            ]
        );
        assert_eq!(
            eviction(&actions),
            (now() - 8 * HOUR + AGE_BUCKET, &Reason::DiskPressure)
        );
    }

    #[test]
    fn pressure_never_touches_recently_used_items() {
        let cache = disk_cache();
        let inventory = Inventory {
            output_bases: vec![base("one_hour", present(), HOUR)],
            caches: vec![cache.clone()],
            ..Inventory::default()
        };
        let usage = Usage {
            sizes: HashMap::from([("/root/one_hour".into(), 1000)]),
            ages: HashMap::from([(cache.root.clone(), BTreeMap::from([(now() - HOUR, 1000)]))]),
        };
        let actions = plan(
            &inventory,
            now(),
            &Config::default(),
            Sweep::Full {
                pressure: Some(Pressure {
                    bytes_to_free: u64::MAX,
                    usage,
                }),
            },
        );
        assert!(deleted_bases(&actions).is_empty());
        assert_eq!(
            eviction(&actions),
            (now() - 72 * HOUR, &Reason::Idle(72 * HOUR))
        );
    }
}
