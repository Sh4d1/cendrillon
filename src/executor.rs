//! Carrying out a plan without racing Bazel.

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};

use crate::inventory::{
    self, Cache, CacheKind, InstallBase, OutputBase, TRASH_PREFIX, disk_cache_entries,
};
use crate::planner::Action;
use crate::sys;

/// How long a Bazel server gets to exit after SIGTERM, and again after SIGKILL.
const SERVER_GRACE: Duration = Duration::from_secs(10);

pub struct Executor<'a> {
    pub dry_run: bool,
    /// Set by a signal handler; checked between deletions.
    pub stop: &'a AtomicBool,
}

enum Outcome {
    Freed(u64),
    Skipped(&'static str),
}

impl Executor<'_> {
    /// Runs `actions` in order, logging each, and returns the bytes freed.
    pub fn run(&self, actions: &[Action]) -> u64 {
        let mut freed = 0;
        for action in actions {
            if self.stopped() {
                break;
            }
            let (what, reason) = describe(action);
            match self.apply(action) {
                Ok(Outcome::Freed(0)) if matches!(action, Action::EvictEntries { .. }) => {}
                Ok(Outcome::Freed(bytes)) => {
                    freed += bytes;
                    let verb = if self.dry_run { "would free" } else { "freed" };
                    tracing::info!("{verb} {} from {what}: {reason}", format_bytes(bytes));
                }
                Ok(Outcome::Skipped(why)) => tracing::info!("kept {what}: {why}"),
                Err(e) => tracing::warn!("failed to clean {what}: {e:#}"),
            }
        }
        freed
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    fn apply(&self, action: &Action) -> Result<Outcome> {
        match action {
            Action::PurgeTrash(path) => self.remove(path).map(Outcome::Freed),
            Action::DeleteOutputBase { base, .. } => self.delete_output_base(base),
            Action::DeleteInstallBase { base, .. } => self.delete_install_base(base),
            Action::EvictEntries {
                cache, older_than, ..
            } => match cache.kind {
                CacheKind::Disk => self.evict_disk_cache(cache, *older_than),
                CacheKind::Repository => self.evict_repository_cache(cache, *older_than),
            },
        }
    }

    /// Takes the output base lock so no Bazel command can start, stops the server, then moves
    /// the base aside and deletes it.
    fn delete_output_base(&self, base: &OutputBase) -> Result<Outcome> {
        // Checked before locking, since locking creates a missing lock file. A command that
        // starts in between holds the lock, so it is still caught below.
        if inventory::last_used(&base.path)? != base.last_used {
            return Ok(Outcome::Skipped(
                "Bazel used it while cendrillon was planning",
            ));
        }
        let Some(lock) = sys::try_lock(&base.path.join("lock"))? else {
            return Ok(Outcome::Skipped("a Bazel command is running"));
        };
        if self.dry_run {
            return Ok(Outcome::Freed(sys::tree_size(&base.path)));
        }
        if let Some(server) = sys::bazel_server(&base.path) {
            sys::terminate(server, SERVER_GRACE)
                .with_context(|| format!("stopping Bazel server {server}"))?;
        }
        let trash = move_to_trash(&base.path)?;
        // A client blocked on the lock sees it unlinked and retries, instead of running in
        // the renamed base.
        fs::remove_file(trash.join("lock"))?;
        drop(lock);
        self.remove(&trash).map(Outcome::Freed)
    }

    /// Mirrors Bazel's own install base GC: lock, check the mtime, rename, drop the lock file.
    fn delete_install_base(&self, base: &InstallBase) -> Result<Outcome> {
        let name = base.path.file_name().context("install base has no name")?;
        let mut lock_name = name.to_os_string();
        lock_name.push(".lock");
        let lock_path = base.path.with_file_name(lock_name);
        let Some(lock) = sys::try_lock(&lock_path)? else {
            return Ok(Outcome::Skipped("a Bazel server is using it"));
        };
        if fs::symlink_metadata(&base.path)?.modified()? != base.last_used {
            return Ok(Outcome::Skipped(
                "Bazel used it while cendrillon was planning",
            ));
        }
        if self.dry_run {
            return Ok(Outcome::Freed(sys::tree_size(&base.path)));
        }
        let trash = move_to_trash(&base.path)?;
        fs::remove_file(&lock_path)?;
        drop(lock);
        self.remove(&trash).map(Outcome::Freed)
    }

    /// Follows Bazel's disk cache GC: holds `gc/lock` so two collections never overlap, skips
    /// `tmp/`, and deletes each entry right after checking its mtime, so an entry a running
    /// build just hit survives. Walking in name order evicts `ac/` before `cas/`.
    fn evict_disk_cache(&self, cache: &Cache, older_than: SystemTime) -> Result<Outcome> {
        let gc_dir = cache.root.join("gc");
        fs::create_dir_all(&gc_dir)?;
        let Some(_lock) = sys::try_lock(&gc_dir.join("lock"))? else {
            return Ok(Outcome::Skipped("Bazel is collecting garbage in it"));
        };
        let mut seen = HashSet::new();
        let mut freed = 0;
        for (path, meta) in disk_cache_entries(&cache.root) {
            if self.stopped() {
                break;
            }
            if meta.modified()? >= older_than {
                continue;
            }
            if !self.dry_run && !remove_if_present(&path)? {
                continue;
            }
            freed += sys::allocated(&meta, &mut seen);
        }
        Ok(Outcome::Freed(freed))
    }

    /// Entries are `content_addressable/<algo>/<hash>/` directories; Bazel touches their
    /// `file` on every hit.
    fn evict_repository_cache(&self, cache: &Cache, older_than: SystemTime) -> Result<Outcome> {
        let mut freed = 0;
        for algo in subdirs(&cache.root.join("content_addressable"))? {
            for entry in subdirs(&algo)? {
                if self.stopped() {
                    return Ok(Outcome::Freed(freed));
                }
                let last_used = children(&entry)?
                    .filter_map(|path| fs::symlink_metadata(path).ok()?.modified().ok())
                    .chain(fs::symlink_metadata(&entry)?.modified())
                    .max();
                if last_used.is_some_and(|time| time < older_than) {
                    freed += self.remove(&entry)?;
                }
            }
        }
        Ok(Outcome::Freed(freed))
    }

    fn remove(&self, path: &Path) -> Result<u64> {
        if self.dry_run {
            return Ok(sys::tree_size(path));
        }
        sys::remove_tree(path).with_context(|| format!("removing {}", path.display()))
    }
}

/// Renames `path` to a hidden sibling, so an interrupted deletion never leaves a half-deleted
/// cache where Bazel would use it, and is finished on a later pass.
fn move_to_trash(path: &Path) -> Result<PathBuf> {
    let name = path.file_name().context("no file name")?.to_string_lossy();
    let stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let trash = path.with_file_name(format!("{TRASH_PREFIX}{name}-{stamp}"));
    fs::rename(path, &trash).with_context(|| format!("renaming {}", path.display()))?;
    Ok(trash)
}

fn remove_if_present(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn children(dir: &Path) -> io::Result<impl Iterator<Item = PathBuf>> {
    Ok(fs::read_dir(dir)?.filter_map(|entry| Some(entry.ok()?.path())))
}

fn subdirs(dir: &Path) -> io::Result<Vec<PathBuf>> {
    match children(dir) {
        Ok(paths) => Ok(paths.filter(|path| path.is_dir()).collect()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

fn describe(action: &Action) -> (String, String) {
    match action {
        Action::PurgeTrash(path) => (
            format!("trash {}", path.display()),
            "finishing an interrupted deletion".into(),
        ),
        Action::DeleteOutputBase { base, reason } => (
            format!("output base {}", base.path.display()),
            reason.to_string(),
        ),
        Action::DeleteInstallBase { base, reason } => (
            format!("install base {}", base.path.display()),
            reason.to_string(),
        ),
        Action::EvictEntries { cache, reason, .. } => {
            let kind = match cache.kind {
                CacheKind::Disk => "disk cache",
                CacheKind::Repository => "repository cache",
            };
            (
                format!("{kind} {}", cache.root.display()),
                reason.to_string(),
            )
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::fixtures::{disk_cache, output_base, set_mtime};
    use crate::inventory::{Item, classify};
    use crate::planner::Reason;
    use crate::sys::testing::LockHolder;

    const HOUR: Duration = Duration::from_secs(3600);
    static RUNNING: AtomicBool = AtomicBool::new(false);

    fn executor(dry_run: bool) -> Executor<'static> {
        Executor {
            dry_run,
            stop: &RUNNING,
        }
    }

    fn found_output_base(path: &Path) -> OutputBase {
        match classify(path) {
            Some(Item::OutputBase(base)) => base,
            other => panic!("expected an output base, found {other:?}"),
        }
    }

    fn delete(base: OutputBase) -> Action {
        Action::DeleteOutputBase {
            base,
            reason: Reason::WorkspaceDeleted("/gone".into()),
        }
    }

    #[test]
    fn deletes_output_base_with_read_only_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = output_base(dir.path(), "base", Path::new("/gone"), SystemTime::now());
        let external = path.join("external/repo");
        fs::create_dir_all(&external).unwrap();
        fs::write(external.join("BUILD"), "").unwrap();
        fs::set_permissions(
            &external,
            std::os::unix::fs::PermissionsExt::from_mode(0o555),
        )
        .unwrap();
        let base = found_output_base(&path);

        assert!(executor(true).run(&[delete(base.clone())]) > 0);
        assert!(path.exists());

        assert!(executor(false).run(&[delete(base)]) > 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn keeps_output_base_used_since_planning() {
        let dir = tempfile::tempdir().unwrap();
        let path = output_base(
            dir.path(),
            "base",
            Path::new("/gone"),
            SystemTime::now() - HOUR,
        );
        let base = found_output_base(&path);
        set_mtime(&path.join("lock"), SystemTime::now());

        assert_eq!(executor(false).run(&[delete(base)]), 0);
        assert!(path.exists());
    }

    #[test]
    fn keeps_output_base_while_a_command_holds_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let path = output_base(dir.path(), "base", Path::new("/gone"), SystemTime::now());
        let base = found_output_base(&path);
        let _client = LockHolder::new(&path.join("lock"));

        assert_eq!(executor(false).run(&[delete(base)]), 0);
        assert!(path.exists());
    }

    #[test]
    fn deletes_install_base_and_its_lock() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install");
        let path = install.join("e15394ac7311f82db7c708c94ae7eeac");
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("A-server.jar"), vec![0u8; 10_000]).unwrap();
        fs::write(install.join("e15394ac7311f82db7c708c94ae7eeac.lock"), "").unwrap();
        let Some(Item::InstallBase(base)) = classify(&path) else {
            panic!("not an install base");
        };
        let action = Action::DeleteInstallBase {
            base,
            reason: Reason::Idle(HOUR),
        };

        assert!(executor(false).run(&[action]) > 0);
        assert_eq!(fs::read_dir(&install).unwrap().count(), 0);
    }

    #[test]
    fn evicts_only_old_disk_cache_entries() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let root = disk_cache(
            dir.path(),
            &[
                ("ac/00/old", now - 100 * HOUR),
                ("cas/00/old", now - 100 * HOUR),
                ("cas/01/new", now),
                ("tmp/old", now - 100 * HOUR),
            ],
        );
        let action = Action::EvictEntries {
            cache: Cache {
                root: root.clone(),
                kind: CacheKind::Disk,
            },
            older_than: now - 72 * HOUR,
            reason: Reason::Idle(72 * HOUR),
        };

        assert!(executor(true).run(std::slice::from_ref(&action)) > 0);
        assert!(root.join("ac/00/old").exists());

        assert!(executor(false).run(&[action]) > 0);
        assert!(!root.join("ac/00/old").exists());
        assert!(!root.join("cas/00/old").exists());
        assert!(root.join("cas/01/new").exists());
        assert!(root.join("tmp/old").exists());
    }

    #[test]
    fn evicts_old_repository_cache_entries() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let sha = dir.path().join("content_addressable/sha256");
        for (hash, mtime) in [("old", now - 100 * HOUR), ("new", now)] {
            fs::create_dir_all(sha.join(hash)).unwrap();
            fs::write(sha.join(hash).join("file"), "x").unwrap();
            set_mtime(&sha.join(hash).join("file"), mtime);
            set_mtime(&sha.join(hash), now - 100 * HOUR);
        }
        let action = Action::EvictEntries {
            cache: Cache {
                root: dir.path().to_path_buf(),
                kind: CacheKind::Repository,
            },
            older_than: now - 72 * HOUR,
            reason: Reason::Idle(72 * HOUR),
        };

        executor(false).run(&[action]);
        assert!(!sha.join("old").exists());
        assert!(sha.join("new/file").exists());
    }

    #[test]
    fn purges_trash_left_by_an_interrupted_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let path = output_base(dir.path(), "base", Path::new("/gone"), SystemTime::now());
        let trash = move_to_trash(&path).unwrap();
        assert_eq!(classify(&trash), Some(Item::Trash(trash.clone())));

        executor(false).run(&[Action::PurgeTrash(trash.clone())]);
        assert!(!trash.exists());
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(999), "999 B");
        assert_eq!(format_bytes(155_000_000_000), "155.0 GB");
    }
}
