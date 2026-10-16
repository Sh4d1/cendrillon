//! What Bazel and Cargo keep on disk: classifying directories and measuring them.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{self, FileType};
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::cargo::{self, Retention, Target};
use crate::sys;

/// Name prefix of a directory renamed for deletion, finished on a later pass if interrupted.
pub const TRASH_PREFIX: &str = ".cendrillon-trash-";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    OutputBase(OutputBase),
    InstallBase(InstallBase),
    Cache(Cache),
    CargoTarget(Target),
    Trash(PathBuf),
}

impl Item {
    pub fn path(&self) -> &Path {
        match self {
            Item::OutputBase(base) => &base.path,
            Item::InstallBase(base) => &base.path,
            Item::Cache(cache) => &cache.root,
            Item::CargoTarget(target) => &target.root,
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
    /// The workspace could not be checked: an unmounted volume, a missing parent directory or a
    /// permission error.
    Unreachable(PathBuf),
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
    /// `--disk_cache`: `{ac,cas}/<2 hex>/<hash>` files, whose mtime Bazel bumps on every hit.
    Disk,
    /// `--repository_cache`: `content_addressable/<algo>/<hash>/file`, touched on every hit.
    /// The `contents/` repo contents cache is left to Bazel's own GC.
    Repository,
}

/// Recognizes a directory Bazel or Cargo manages from the markers they leave in it. The checks
/// are strict, because whatever matches under the home directory may be deleted.
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
    if in_install_dir && is_hex(name, 32) && is_file(&dir.join("build-label.txt")) {
        return Some(Item::InstallBase(InstallBase {
            path: dir.to_path_buf(),
            last_used: mtime(dir).ok()?,
        }));
    }
    if let Some(target) = Target::classify(dir) {
        return Some(Item::CargoTarget(target));
    }
    let is_shard = |name: &str, kind: FileType| kind.is_dir() && is_hex(name, 2);
    let kind = if ["ac", "cas"]
        .iter()
        .all(|sub| only_children(&dir.join(sub), is_shard))
    {
        CacheKind::Disk
    } else if only_children(&dir.join("content_addressable"), |name, kind| {
        kind.is_dir() && HASH_FUNCTIONS.contains(&name)
    }) {
        CacheKind::Repository
    } else {
        return None;
    };
    Some(Item::Cache(Cache {
        root: dir.to_path_buf(),
        kind,
    }))
}

const HASH_FUNCTIONS: [&str; 5] = ["sha1", "sha256", "sha384", "sha512", "blake3"];

/// The workspace of an output base, or `None` if `dir` is not one. Bazel records the absolute
/// workspace path in two files; both must agree.
fn recorded_workspace(dir: &Path) -> Option<Workspace> {
    let marker = fs::read_to_string(dir.join("DO_NOT_BUILD_HERE")).ok()?;
    let readme = fs::read_to_string(dir.join("README")).ok()?;
    let path = Path::new(marker.trim());
    let readme_path = readme.lines().next()?.strip_prefix("WORKSPACE: ")?;
    (path.is_absolute() && Path::new(readme_path) == path)
        .then(|| Workspace::check(path.to_path_buf()))
}

/// True when `dir` is a real directory whose entries, ignoring hidden ones such as
/// `.DS_Store`, all pass `allowed`.
fn only_children(dir: &Path, allowed: impl Fn(&str, FileType) -> bool) -> bool {
    if !fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir()) {
        return false;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.into_iter().all(|entry| {
        entry.is_ok_and(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with('.') || entry.file_type().is_ok_and(|kind| allowed(&name, kind))
        })
    })
}

impl Workspace {
    fn check(path: PathBuf) -> Self {
        match fs::symlink_metadata(&path) {
            Ok(_) => Workspace::Present(path),
            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    && path.parent().is_some_and(Path::is_dir)
                    && volume_mounted(&path) =>
            {
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

fn is_hex(name: &str, len: usize) -> bool {
    name.len() == len && name.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
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
    pub targets: Vec<Target>,
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
                Item::CargoTarget(target) => inventory.targets.push(target),
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
        let targets = self.targets.iter().cloned().map(Item::CargoTarget);
        let trash = self.trash.iter().cloned().map(Item::Trash);
        bases
            .chain(installs)
            .chain(caches)
            .chain(targets)
            .chain(trash)
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
    /// Per Cargo target: bytes of build units nothing current depends on, which trimming
    /// frees. Included in `sizes`.
    pub garbage: HashMap<PathBuf, u64>,
}

impl Usage {
    /// Walks the given items. Slow on large caches: run it only when sizes matter.
    pub fn measure(items: impl IntoIterator<Item = Item>, cargo: Retention) -> Self {
        let mut usage = Usage::default();
        for item in items {
            let path = item.path().to_path_buf();
            let size = match &item {
                Item::Cache(cache) if cache.kind == CacheKind::Disk => {
                    let ages = disk_cache_ages(&cache.root);
                    let size = ages.values().sum();
                    usage.ages.insert(path.clone(), ages);
                    size
                }
                Item::CargoTarget(target) => {
                    if let Ok(garbage) = cargo::garbage(&target.root, cargo, SystemTime::now()) {
                        let bytes = garbage.paths.iter().map(|path| sys::unshared_size(path));
                        usage.garbage.insert(path.clone(), bytes.sum());
                    }
                    sys::tree_size(&path)
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

    pub fn garbage(&self, path: &Path) -> u64 {
        self.garbage.get(path).copied().unwrap_or(0)
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

/// Files of a disk cache: `{ac,cas}/<2 hex>/<hash>`, `ac/` first. Like Bazel's GC, this
/// leaves `tmp/` and `gc/` alone.
pub fn disk_cache_entries(root: &Path) -> impl Iterator<Item = (PathBuf, fs::Metadata)> {
    ["ac", "cas"]
        .into_iter()
        .flat_map(move |sub| subdirs(&root.join(sub), |name| is_hex(name, 2)))
        .flat_map(|shard| {
            let mut files: Vec<_> = fs::read_dir(shard)
                .into_iter()
                .flatten()
                .filter_map(|entry| {
                    let entry = entry.ok()?;
                    let meta = entry.metadata().ok()?;
                    meta.is_file().then(|| (entry.path(), meta))
                })
                .collect();
            files.sort_by(|a, b| a.0.cmp(&b.0));
            files
        })
}

/// Entries of a repository cache: `content_addressable/<algo>/<hex hash>/` directories.
pub fn repository_cache_entries(root: &Path) -> impl Iterator<Item = PathBuf> {
    subdirs(&root.join("content_addressable"), |name| {
        HASH_FUNCTIONS.contains(&name)
    })
    .into_iter()
    .flat_map(|algo| {
        subdirs(&algo, |name| {
            !name.is_empty() && name.bytes().all(|b| b.is_ascii_hexdigit())
        })
    })
}

/// Subdirectories of `dir` with an accepted name, sorted. Symlinks, including `dir` itself,
/// are not followed.
fn subdirs(dir: &Path, accepted: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    if !fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir()) {
        return Vec::new();
    }
    let mut dirs: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_dir())
                && entry.file_name().to_str().is_some_and(&accepted)
        })
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    dirs
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
        for dir in ["ac/00", "cas/00", "tmp", "gc"] {
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
        fs::create_dir(dir.path().join("worktrees")).unwrap();
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
    fn output_base_needs_both_markers_naming_the_same_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let base = output_base(dir.path(), "base", Path::new("/gone"), SystemTime::now());
        assert!(matches!(classify(&base), Some(Item::OutputBase(_))));

        fs::write(base.join("DO_NOT_BUILD_HERE"), "/other").unwrap();
        assert_eq!(classify(&base), None);

        fs::write(base.join("DO_NOT_BUILD_HERE"), "see README").unwrap();
        fs::write(base.join("README"), "WORKSPACE: see README\n").unwrap();
        assert_eq!(classify(&base), None);

        fs::remove_file(base.join("DO_NOT_BUILD_HERE")).unwrap();
        assert_eq!(classify(&base), None);
    }

    #[test]
    fn folders_that_merely_resemble_caches_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("notes");
        fs::create_dir_all(disk.join("ac/00")).unwrap();
        fs::create_dir_all(disk.join("cas")).unwrap();
        fs::write(disk.join("cas/todo.txt"), "").unwrap();
        assert_eq!(classify(&disk), None);

        let repo = dir.path().join("repo");
        fs::create_dir_all(repo.join("content_addressable/photos")).unwrap();
        assert_eq!(classify(&repo), None);

        let install = dir.path().join("install/e15394ac7311f82db7c708c94ae7eeac");
        fs::create_dir_all(&install).unwrap();
        assert_eq!(classify(&install), None);
    }

    #[test]
    fn classifies_install_base_caches_and_trash() {
        let dir = tempfile::tempdir().unwrap();
        let install = dir.path().join("install/e15394ac7311f82db7c708c94ae7eeac");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("build-label.txt"), "9.2.0").unwrap();
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
        let usage = Usage::measure(
            inventory.items(),
            crate::config::Config::default().cargo_retention(),
        );
        let ages = &usage.ages[&cache];
        assert_eq!(ages.len(), 2);
        assert_eq!(ages[&bucket_start(old)], 2 * ages[&bucket_start(now)]);
        assert_eq!(usage.size(&cache), ages.values().sum::<u64>());
    }
}
