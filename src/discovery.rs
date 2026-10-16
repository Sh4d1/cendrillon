//! Finding Bazel caches: the default locations, paths named in bazelrc files, and a walk of
//! the home directory for the rest.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::env::Env;
use crate::inventory::{Inventory, Item, OutputBase, TRASH_PREFIX, Workspace, classify};

/// Deep enough to reach `<root>/<output_user_root>/install/<md5>`.
const ROOT_DEPTH: usize = 3;

/// Bazel flags whose value is a directory Bazel fills with caches.
const PATH_FLAGS: [&str; 5] = [
    "output_user_root",
    "output_base",
    "install_base",
    "disk_cache",
    "repository_cache",
];

/// Directories never walked: they are huge and hold no cache (`.git`, `node_modules`), belong
/// to a tool that manages them (`.rustup` toolchains), or reading them makes macOS ask the
/// user for permission.
const SKIPPED_NAMES: [&str; 4] = [".git", "node_modules", ".Trash", ".rustup"];
const SKIPPED_HOME_DIRS: [&str; 8] = [
    "Desktop",
    "Documents",
    "Downloads",
    "Library",
    "Movies",
    "Music",
    "Pictures",
    "Public",
];

/// What the home walk found, kept between runs.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub last_home_scan: Option<SystemTime>,
    /// Directories to search on every pass, besides the defaults.
    pub roots: BTreeSet<PathBuf>,
}

impl State {
    /// Loads the state, or starts afresh when it is missing or unreadable.
    pub fn load(path: &Path) -> Self {
        let Ok(text) = fs::read_to_string(path) else {
            return Self::default();
        };
        serde_json::from_str(&text).unwrap_or_else(|e| {
            tracing::warn!("ignoring corrupt state {}: {e}", path.display());
            Self::default()
        })
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let dir = path.parent().context("state file has no parent")?;
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, serde_json::to_vec_pretty(self)?)
            .with_context(|| format!("writing {}", temp.display()))?;
        fs::rename(&temp, path).with_context(|| format!("writing {}", path.display()))
    }
}

/// Everything Bazel and Cargo keep on disk that cendrillon knows how to find.
pub fn discover(env: &Env, extra_roots: &[PathBuf], state: &State) -> Inventory {
    let mut roots: BTreeSet<PathBuf> = default_roots(env).collect();
    roots.extend(extra_roots.iter().cloned());
    roots.extend(state.roots.iter().filter(|root| !is_skipped(root)).cloned());
    let global_rcs = [
        env.home.join(".bazelrc"),
        PathBuf::from("/etc/bazel.bazelrc"),
    ];
    for rc in &global_rcs {
        roots.extend(bazelrc_paths(rc, None, &env.home));
    }
    let mut items: Vec<Item> = roots
        .iter()
        .flat_map(|root| walk(root, ROOT_DEPTH, &env.home))
        .collect();

    let workspaces: BTreeSet<&Path> = items
        .iter()
        .filter_map(|item| match item {
            Item::OutputBase(OutputBase {
                workspace: Workspace::Present(workspace),
                ..
            }) => Some(workspace.as_path()),
            _ => None,
        })
        .collect();
    let workspace_roots: BTreeSet<PathBuf> = workspaces
        .into_iter()
        .flat_map(|workspace| {
            bazelrc_paths(&workspace.join(".bazelrc"), Some(workspace), &env.home)
        })
        .filter(|root| !roots.contains(root))
        .collect();
    items.extend(
        workspace_roots
            .iter()
            .flat_map(|root| walk(root, ROOT_DEPTH, &env.home)),
    );
    // A deletion of a whole root (a Cargo target) cut short leaves its trash beside it.
    items.extend(
        roots
            .iter()
            .filter(|root| fs::symlink_metadata(root).is_err())
            .flat_map(|root| trash_beside(root)),
    );
    items.into_iter().collect()
}

fn is_skipped(path: &Path) -> bool {
    path.components()
        .any(|part| SKIPPED_NAMES.iter().any(|skip| part.as_os_str() == *skip))
}

/// What renaming `root` to trash produced: `<parent>/.cendrillon-trash-<name>-<stamp>`.
fn trash_beside(root: &Path) -> Vec<Item> {
    let (Some(parent), Some(name)) = (root.parent(), root.file_name()) else {
        return Vec::new();
    };
    let prefix = format!("{TRASH_PREFIX}{}-", name.to_string_lossy());
    fs::read_dir(parent)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with(&prefix)
                && entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| Item::Trash(entry.path()))
        .collect()
}

/// Where Bazel puts caches unless told otherwise, plus the usual macOS cache directory.
fn default_roots(env: &Env) -> impl Iterator<Item = PathBuf> {
    [
        PathBuf::from(format!("/private/var/tmp/_bazel_{}", env.user)),
        env.home.join("Library/Caches/bazel"),
        env.home.join(".cache/bazel"),
    ]
    .into_iter()
}

/// Walks the home directory for Bazel caches, returning the directories to search from now
/// on. Skips the folders macOS protects, so no Full Disk Access is needed.
pub fn scan_home(env: &Env) -> BTreeSet<PathBuf> {
    let caches = env.home.join("Library/Caches");
    [env.home.as_path(), caches.as_path()]
        .into_iter()
        .flat_map(|root| walk(root, usize::MAX, &env.home))
        .filter_map(|item| search_root(&item))
        .collect()
}

/// The directory to search to find `item` again, along with its siblings: the output user
/// root for output bases and install bases, the directory itself for caches and targets.
fn search_root(item: &Item) -> Option<PathBuf> {
    match item {
        Item::OutputBase(_) | Item::Trash(_) => item.path().parent().map(Path::to_path_buf),
        Item::InstallBase(_) => item.path().parent()?.parent().map(Path::to_path_buf),
        Item::Cache(cache) => Some(cache.root.clone()),
        Item::CargoTarget(target) => Some(target.root.clone()),
    }
}

/// Classifies directories under `root`, not descending into what it recognizes, nor into
/// other volumes.
///
/// A skipped directory is never opened, not even listed: reading a protected folder such as
/// `~/Documents` is what makes macOS ask the user for permission. Walkers like `walkdir`
/// list a directory before letting the caller skip it, hence this hand-written recursion.
fn walk(root: &Path, max_depth: usize, home: &Path) -> Vec<Item> {
    let Ok(root) = fs::canonicalize(root) else {
        return Vec::new();
    };
    let Ok(device) = fs::metadata(&root).map(|meta| meta.dev()) else {
        return Vec::new();
    };
    let home = fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let skipped_home_dirs: HashSet<PathBuf> = SKIPPED_HOME_DIRS
        .iter()
        .map(|name| home.join(name))
        .collect();
    let mut items = Vec::new();
    let mut pending = vec![(root, 0)];
    while let Some((dir, depth)) = pending.pop() {
        if let Some(item) = classify(&dir) {
            items.push(item);
            continue;
        }
        if depth == max_depth {
            continue;
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            // From the directory listing itself: no symlink is followed, nothing is opened.
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let path = entry.path();
            let name = entry.file_name();
            if SKIPPED_NAMES.iter().any(|skip| name == *skip) || skipped_home_dirs.contains(&path) {
                continue;
            }
            if entry.metadata().is_ok_and(|meta| meta.dev() == device) {
                pending.push((path, depth + 1));
            }
        }
    }
    items
}

/// Cache directories named by path flags in a bazelrc file and the files it imports. Flags
/// under every `--config` count, since any of them may have been used.
fn bazelrc_paths(file: &Path, workspace: Option<&Path>, home: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let mut visited = HashSet::new();
    collect_bazelrc_paths(file, workspace, home, &mut visited, &mut paths);
    paths
}

fn collect_bazelrc_paths(
    file: &Path,
    workspace: Option<&Path>,
    home: &Path,
    visited: &mut HashSet<PathBuf>,
    paths: &mut Vec<PathBuf>,
) {
    if !visited.insert(file.to_path_buf()) {
        return;
    }
    let Ok(text) = fs::read_to_string(file) else {
        return;
    };
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default();
        let mut words = line.split_whitespace();
        let Some(command) = words.next() else {
            continue;
        };
        if command == "import" || command == "try-import" {
            if let Some(imported) = words.next().and_then(|w| expand(w, workspace, home)) {
                collect_bazelrc_paths(&imported, workspace, home, visited, paths);
            }
            continue;
        }
        let args: Vec<&str> = words.collect();
        for (i, arg) in args.iter().enumerate() {
            let Some(flag) = arg.strip_prefix("--") else {
                continue;
            };
            let (name, value) = match flag.split_once('=') {
                Some((name, value)) => (name, Some(value).filter(|v| !v.is_empty())),
                None => (
                    flag,
                    args.get(i + 1).copied().filter(|v| !v.starts_with("--")),
                ),
            };
            if PATH_FLAGS.contains(&name)
                && let Some(path) = value.and_then(|v| expand(v, workspace, home))
            {
                paths.push(path);
            }
        }
    }
}

/// Resolves `~/` and `%workspace%` as Bazel does. Relative paths are dropped: they depend on
/// the directory Bazel ran in.
fn expand(value: &str, workspace: Option<&Path>, home: &Path) -> Option<PathBuf> {
    let value = value.trim_matches(['"', '\'']);
    let path = if let Some(rest) = value.strip_prefix("~/") {
        home.join(rest)
    } else if let Some(rest) = value.strip_prefix("%workspace%") {
        workspace?.join(rest.trim_start_matches('/'))
    } else {
        PathBuf::from(value)
    };
    path.is_absolute().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::fixtures::{disk_cache, output_base};

    fn env(dir: &Path) -> Env {
        Env {
            home: fs::canonicalize(dir).unwrap(),
            user: "cendrillon-test-user".into(),
        }
    }

    #[test]
    fn parses_path_flags_from_bazelrc_and_imports() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let workspace = home.join("repo");
        fs::create_dir(&workspace).unwrap();
        fs::write(
            workspace.join(".bazelrc"),
            "common:macos --disk_cache=~/Library/Caches/bazel/_bazel_disk_cache\n\
             common:linux --disk_cache=~/.cache/bazel/_bazel_disk_cache # comment\n\
             common:ci --disk_cache=\n\
             startup --output_user_root /tmp/bazel-root\n\
             build --repository_cache=relative/path --jobs=8\n\
             try-import %workspace%/user.bazelrc\n",
        )
        .unwrap();
        fs::write(
            workspace.join("user.bazelrc"),
            "build --disk_cache=/big/cache\n",
        )
        .unwrap();

        let paths = bazelrc_paths(&workspace.join(".bazelrc"), Some(&workspace), home);
        assert_eq!(
            paths,
            [
                home.join("Library/Caches/bazel/_bazel_disk_cache"),
                home.join(".cache/bazel/_bazel_disk_cache"),
                PathBuf::from("/tmp/bazel-root"),
                PathBuf::from("/big/cache"),
            ]
        );
    }

    #[test]
    fn discovers_default_root_and_caches_named_by_workspace_bazelrc() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let workspace = env.home.join("work/repo");
        fs::create_dir_all(&workspace).unwrap();
        let elsewhere = env.home.join("elsewhere/disk");
        fs::write(
            workspace.join(".bazelrc"),
            format!("build --disk_cache={}\n", elsewhere.display()),
        )
        .unwrap();
        let user_root = env.home.join("Library/Caches/bazel/_bazel_test");
        let now = SystemTime::now();
        output_base(&user_root, "aaaa", &workspace, now);
        let install = user_root.join("install/e15394ac7311f82db7c708c94ae7eeac");
        fs::create_dir_all(&install).unwrap();
        fs::write(install.join("build-label.txt"), "9.2.0").unwrap();
        disk_cache(&elsewhere, &[]);

        let inventory = discover(&env, &[], &State::default());
        assert_eq!(inventory.output_bases.len(), 1);
        assert_eq!(inventory.install_bases.len(), 1);
        assert_eq!(inventory.caches.len(), 1);
        assert_eq!(inventory.caches[0].root, elsewhere);
    }

    #[test]
    fn home_scan_finds_caches_and_skips_protected_folders() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let now = SystemTime::now();
        output_base(
            &env.home.join("odd/place/root"),
            "bbbb",
            Path::new("/x"),
            now,
        );
        disk_cache(&env.home.join("Documents/cache"), &[]);
        disk_cache(&env.home.join("src/node_modules/cache"), &[]);
        disk_cache(&env.home.join("Library/Caches/other/disk"), &[]);
        disk_cache(&env.home.join("Library/Developer/disk"), &[]);

        let roots = scan_home(&env);
        assert_eq!(
            roots,
            BTreeSet::from([
                env.home.join("odd/place/root"),
                env.home.join("Library/Caches/other/disk"),
            ])
        );

        let state = State {
            last_home_scan: Some(now),
            roots,
        };
        let inventory = discover(&env, &[], &state);
        assert_eq!(inventory.output_bases.len(), 1);
        assert_eq!(inventory.caches.len(), 1);
    }

    #[test]
    fn home_scan_follows_no_symlink_and_survives_unreadable_skipped_folders() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let outside = tempfile::tempdir().unwrap();
        disk_cache(&outside.path().join("disk"), &[]);
        std::os::unix::fs::symlink(outside.path(), env.home.join("link")).unwrap();
        let documents = env.home.join("Documents");
        disk_cache(&documents.join("cache"), &[]);
        let mode = |m| std::os::unix::fs::PermissionsExt::from_mode(m);
        fs::set_permissions(&documents, mode(0o000)).unwrap();
        disk_cache(&env.home.join("src/a/b/c/disk"), &[]);

        let roots = scan_home(&env);
        fs::set_permissions(&documents, mode(0o755)).unwrap();
        assert_eq!(roots, BTreeSet::from([env.home.join("src/a/b/c/disk")]));
    }

    #[test]
    fn finds_the_trash_of_a_root_deleted_midway() {
        let dir = tempfile::tempdir().unwrap();
        let env = env(dir.path());
        let trash = env.home.join("src/repo/.cendrillon-trash-target-1700");
        fs::create_dir_all(trash.join("debug")).unwrap();
        let state = State {
            last_home_scan: None,
            roots: BTreeSet::from([env.home.join("src/repo/target")]),
        };
        assert_eq!(discover(&env, &[], &state).trash, [trash]);
    }

    #[test]
    fn state_round_trips_and_tolerates_corruption() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/state.json");
        assert_eq!(State::load(&path), State::default());
        let state = State {
            last_home_scan: Some(SystemTime::UNIX_EPOCH),
            roots: BTreeSet::from([PathBuf::from("/a")]),
        };
        state.save(&path).unwrap();
        assert_eq!(State::load(&path), state);
        fs::write(&path, "{").unwrap();
        assert_eq!(State::load(&path), State::default());
    }
}
