//! What Bazel keeps on disk: classifying directories and measuring them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use walkdir::WalkDir;

use crate::sys;

/// Name prefix of a directory renamed for deletion, finished on a later pass if interrupted.
pub const TRASH_PREFIX: &str = ".cendrillon-trash-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    OutputBase(OutputBase),
    InstallBase(InstallBase),
    Cache(Cache),
    Trash(PathBuf),
}

impl Item {
    pub fn path(&self) -> &Path {
        match self {
            Item::OutputBase(base) => &base.path,
            Item::InstallBase(base) => &base.path,
            Item::Cache(cache) => &cache.root,
            Item::Trash(path) => path,
        }
    }
}

/// A Bazel output base: `<output_user_root>/<md5 of workspace path>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputBase {
    pub path: PathBuf,
    pub workspace: Workspace,
    /// When a Bazel client last ran a command here: it rewrites `<base>/lock` every time.
    pub last_used: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workspace {
    Present(PathBuf),
    /// The workspace is gone, typically a deleted worktree; its output base is useless.
    Missing(PathBuf),
    /// The workspace could not be checked: an unmounted volume or a permission error.
    Unreachable(PathBuf),
    /// The output base does not record its workspace.
    Unrecorded,
}

/// An extracted Bazel binary: `<output_user_root>/install/<md5>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallBase {
    pub path: PathBuf,
    /// The Bazel client touches the directory on every invocation.
    pub last_used: SystemTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cache {
    pub root: PathBuf,
    pub kind: CacheKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheKind {
    /// `--disk_cache`: `ac/` and `cas/` files, whose mtime Bazel bumps on every hit.
    Disk,
    /// `--repository_cache`: `content_addressable/<algo>/<hash>/file`, touched on every hit.
    /// The `contents/` repo contents cache is left to Bazel's own GC.
    Repository,
}

/// Recognizes a Bazel-managed directory from the markers Bazel leaves in it.
pub fn classify(dir: &Path) -> Option<Item> {
    let name = dir.file_name()?.to_str()?;
    if name.starts_with(TRASH_PREFIX) {
        return Some(Item::Trash(dir.to_path_buf()));
    }
    if let Some(workspace) = recorded_workspace(dir) {
        return Some(Item::OutputBase(OutputBase {
            path: dir.to_path_buf(),
            workspace,
            last_used: last_used(dir).ok()?,
        }));
    }
    let in_install_dir = dir.parent()?.file_name() == Some("install".as_ref());
    if in_install_dir && is_md5(name) {
        return Some(Item::InstallBase(InstallBase {
            path: dir.to_path_buf(),
            last_used: mtime(dir).ok()?,
        }));
    }
    let kind = if dir.join("ac").is_dir() && dir.join("cas").is_dir() {
        CacheKind::Disk
    } else if dir.join("content_addressable").is_dir() {
        CacheKind::Repository
    } else {
        return None;
    };
    Some(Item::Cache(Cache {
        root: dir.to_path_buf(),
        kind,
    }))
}

/// The workspace an output base records, or `None` if `dir` is not an output base.
fn recorded_workspace(dir: &Path) -> Option<Workspace> {
    if let Ok(text) = fs::read_to_string(dir.join("DO_NOT_BUILD_HERE")) {
        let path = text.trim();
        return Some(if path.is_empty() {
            Workspace::Unrecorded
        } else {
            Workspace::check(PathBuf::from(path))
        });
    }
    let readme = fs::read_to_string(dir.join("README")).ok()?;
    let path = readme.lines().next()?.strip_prefix("WORKSPACE: ")?;
    Some(Workspace::check(PathBuf::from(path)))
}

impl Workspace {
    fn check(path: PathBuf) -> Self {
        match fs::symlink_metadata(&path) {
            Ok(_) => Workspace::Present(path),
            Err(e) if e.kind() == io::ErrorKind::NotFound && volume_mounted(&path) => {
                Workspace::Missing(path)
            }
            Err(_) => Workspace::Unreachable(path),
        }
    }
}

/// False when `path` lies on a volume under `/Volumes` that is not mounted right now.
fn volume_mounted(path: &Path) -> bool {
    let mut components = path.components();
    let (Some(Component::RootDir), Some(Component::Normal(top)), Some(Component::Normal(name))) =
        (components.next(), components.next(), components.next())
    else {
        return true;
    };
    if top != "Volumes" {
        return true;
    }
    let volumes = Path::new("/Volumes");
    match (fs::metadata(volumes), fs::metadata(volumes.join(name))) {
        (Ok(parent), Ok(volume)) => parent.dev() != volume.dev(),
        _ => false,
    }
}

fn is_md5(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// When a Bazel command last ran in an output base.
pub fn last_used(output_base: &Path) -> io::Result<SystemTime> {
    mtime(&output_base.join("lock")).or_else(|_| mtime(output_base))
}

fn mtime(path: &Path) -> io::Result<SystemTime> {
    fs::symlink_metadata(path)?.modified()
}

/// Everything found, deduplicated by path.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Inventory {
    pub output_bases: Vec<OutputBase>,
    pub install_bases: Vec<InstallBase>,
    pub caches: Vec<Cache>,
    pub trash: Vec<PathBuf>,
}

impl FromIterator<Item> for Inventory {
    fn from_iter<I: IntoIterator<Item = Item>>(items: I) -> Self {
        let unique: BTreeMap<PathBuf, Item> = items
            .into_iter()
            .map(|item| (item.path().to_path_buf(), item))
            .collect();
        let mut inventory = Inventory::default();
        for item in unique.into_values() {
            match item {
                Item::OutputBase(base) => inventory.output_bases.push(base),
                Item::InstallBase(base) => inventory.install_bases.push(base),
                Item::Cache(cache) => inventory.caches.push(cache),
                Item::Trash(path) => inventory.trash.push(path),
            }
        }
        inventory
    }
}

impl Inventory {
    pub fn items(&self) -> impl Iterator<Item = Item> + '_ {
        let bases = self.output_bases.iter().cloned().map(Item::OutputBase);
        let installs = self.install_bases.iter().cloned().map(Item::InstallBase);
        let caches = self.caches.iter().cloned().map(Item::Cache);
        let trash = self.trash.iter().cloned().map(Item::Trash);
        bases.chain(installs).chain(caches).chain(trash)
    }
}

/// Width of the buckets disk-cache entries are grouped into by last use.
pub const AGE_BUCKET: Duration = Duration::from_secs(600);

/// Bytes on disk per item, and for disk caches, how those bytes spread over last-use time.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Usage {
    pub sizes: HashMap<PathBuf, u64>,
    /// Per disk cache: bytes per [`AGE_BUCKET`], keyed by the bucket's start.
    pub ages: HashMap<PathBuf, BTreeMap<SystemTime, u64>>,
}

impl Usage {
    /// Walks every item. Slow on large caches: run it only when sizes matter.
    pub fn measure(inventory: &Inventory) -> Self {
        let mut usage = Usage::default();
        for item in inventory.items() {
            let path = item.path().to_path_buf();
            let size = match &item {
                Item::Cache(cache) if cache.kind == CacheKind::Disk => {
                    let ages = disk_cache_ages(&cache.root);
                    let size = ages.values().sum();
                    usage.ages.insert(path.clone(), ages);
                    size
                }
                _ => sys::tree_size(&path),
            };
            usage.sizes.insert(path, size);
        }
        usage
    }

    pub fn size(&self, path: &Path) -> u64 {
        self.sizes.get(path).copied().unwrap_or(0)
    }
}

fn disk_cache_ages(root: &Path) -> BTreeMap<SystemTime, u64> {
    let mut seen = HashSet::new();
    let mut ages = BTreeMap::new();
    for (_, meta) in disk_cache_entries(root) {
        let Ok(modified) = meta.modified() else {
            continue;
        };
        *ages.entry(bucket_start(modified)).or_default() += sys::allocated(&meta, &mut seen);
    }
    ages
}

/// Files of a disk cache, skipping the `tmp/` and `gc/` directories as Bazel's GC does.
pub fn disk_cache_entries(root: &Path) -> impl Iterator<Item = (PathBuf, fs::Metadata)> {
    let excluded = [root.join("tmp"), root.join("gc")];
    WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .filter_entry(move |entry| !excluded.iter().any(|dir| dir == entry.path()))
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter_map(|entry| Some((entry.path().to_path_buf(), entry.metadata().ok()?)))
}

pub fn bucket_start(time: SystemTime) -> SystemTime {
    let since_epoch = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    SystemTime::UNIX_EPOCH + Duration::from_secs(since_epoch - since_epoch % AGE_BUCKET.as_secs())
}

#[cfg(test)]
pub mod fixtures {
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    use filetime::FileTime;

    pub fn set_mtime(path: &Path, time: SystemTime) {
        filetime::set_symlink_file_times(
            path,
            FileTime::from_system_time(time),
            FileTime::from_system_time(time),
        )
        .unwrap();
    }

    /// Lays out an output base the way Bazel does.
    pub fn output_base(
        root: &Path,
        name: &str,
        workspace: &Path,
        last_used: SystemTime,
    ) -> PathBuf {
        let base = root.join(name);
        std::fs::create_dir_all(base.join("execroot/_main")).unwrap();
        std::fs::write(
            base.join("DO_NOT_BUILD_HERE"),
            workspace.as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        std::fs::write(
            base.join("README"),
            format!("WORKSPACE: {}\n", workspace.display()),
        )
        .unwrap();
        std::fs::write(base.join("execroot/_main/out.o"), vec![1u8; 50_000]).unwrap();
        std::fs::write(base.join("lock"), "pid=1\nowner=client\n").unwrap();
        set_mtime(&base.join("lock"), last_used);
        base
    }

    pub fn disk_cache(root: &Path, entries: &[(&str, SystemTime)]) -> PathBuf {
        for dir in ["ac", "cas", "tmp", "gc"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        for (path, mtime) in entries {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, vec![2u8; 10_000]).unwrap();
            set_mtime(&path, *mtime);
        }
        root.to_path_buf()
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    const HOUR: Duration = Duration::from_secs(3600);

    #[test]
    fn classifies_output_base_with_its_workspace_and_last_use() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("repo");
        fs::create_dir(&workspace).unwrap();
        let used = SystemTime::now() - 5 * HOUR;
        let base = output_base(
            dir.path(),
            "6bd1ce09a0bd49b92259e5942cc14dbd",
            &workspace,
            used,
        );

        let Some(Item::OutputBase(found)) = classify(&base) else {
            panic!("not an output base");
        };
        assert_eq!(found.workspace, Workspace::Present(workspace));
        let secs = |t: SystemTime| t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        assert_eq!(secs(found.last_used), secs(used));
    }

    #[test]
    fn deleted_worktree_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("worktrees/gone");
        let base = output_base(dir.path(), "base", &gone, SystemTime::now());
        let Some(Item::OutputBase(found)) = classify(&base) else {
            panic!("not an output base");
        };
        assert_eq!(found.workspace, Workspace::Missing(gone));
    }

    #[test]
    fn workspace_on_unmounted_volume_is_unreachable() {
        let path = PathBuf::from("/Volumes/cendrillon-not-mounted/repo");
        assert_eq!(Workspace::check(path.clone()), Workspace::Unreachable(path));
    }

    #[test]
    fn falls_back_to_readme_for_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let base = output_base(
            dir.path(),
            "base",
            Path::new("/nonexistent"),
            SystemTime::now(),
        );
        fs::remove_file(base.join("DO_NOT_BUILD_HERE")).unwrap();
        let Some(Item::OutputBase(found)) = classify(&base) else {
            panic!("not an output base");
        };
        assert_eq!(found.workspace, Workspace::Missing("/nonexistent".into()));
    }

    #[test]
    fn classifies_install_base_caches_and_trash() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install/e15394ac7311f82db7c708c94ae7eeac");
        fs::create_dir_all(&install).unwrap();
        assert!(matches!(classify(&install), Some(Item::InstallBase(_))));
        assert_eq!(classify(&dir.path().join("install")), None);

        let disk = disk_cache(&dir.path().join("disk"), &[]);
        assert!(matches!(
            classify(&disk),
            Some(Item::Cache(Cache {
                kind: CacheKind::Disk,
                ..
            }))
        ));

        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("content_addressable/sha256")).unwrap();
        assert!(matches!(
            classify(&repo),
            Some(Item::Cache(Cache {
                kind: CacheKind::Repository,
                ..
            }))
        ));

        let trash = output_base(
            dir.path(),
            ".cendrillon-trash-x",
            Path::new("/x"),
            SystemTime::now(),
        );
        assert_eq!(classify(&trash), Some(Item::Trash(trash)));
    }

    #[test]
    fn measures_disk_cache_by_age_and_skips_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let now = SystemTime::now();
        let old = now - 100 * HOUR;
        let cache = disk_cache(
            dir.path(),
            &[
                ("ac/00/aa", old),
                ("cas/00/bb", old),
                ("cas/01/cc", now),
                ("tmp/partial", old),
            ],
        );
        let inventory: Inventory = classify(&cache).into_iter().collect();
        let usage = Usage::measure(&inventory);
        let ages = &usage.ages[&cache];
        assert_eq!(ages.len(), 2);
        assert_eq!(ages[&bucket_start(old)], 2 * ages[&bucket_start(now)]);
        assert_eq!(usage.size(&cache), ages.values().sum::<u64>());
    }
}
