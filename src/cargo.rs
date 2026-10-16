//! Cargo target directories: recognizing them, and telling the build units still in use from
//! the ones nothing current depends on.
//!
//! Cargo never deletes a build unit. Every change of features, flags, toolchain or dependency
//! versions compiles new generations of the affected crates next to the old ones. Cargo records
//! no last-use time cendrillon could read (a Fresh build reads fingerprint files without writing
//! them, and macOS does not refresh their access time), so liveness comes from the unit graph:
//! each unit's `.fingerprint/<unit>/<role>.json` lists the fingerprints of the units it was
//! built against. The newest generation of every top-level unit, and everything it depends
//! on, is in use. The rest is garbage.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::Deserialize;

/// A Cargo target directory, `CARGO_TARGET_DIR`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub root: PathBuf,
    /// When Cargo last compiled anything here or ran a build, even a Fresh one: rustc writes
    /// outputs into `deps/` through renames, and a build rewrites the dep-info of its uplifted
    /// outputs in the profile directory.
    pub last_used: SystemTime,
}

impl Target {
    /// Recognizes a target directory: Cargo's `.rustc_info.json` next to at least one profile
    /// directory holding `.fingerprint/` and `deps/`.
    pub fn classify(dir: &Path) -> Option<Self> {
        if !is_file(&dir.join(".rustc_info.json")) {
            return None;
        }
        let last_used = profiles(dir)
            .iter()
            .flat_map(|profile| {
                [
                    profile.clone(),
                    profile.join("deps"),
                    profile.join(".fingerprint"),
                ]
            })
            .filter_map(|path| mtime(&path))
            .max()?;
        Some(Self {
            root: dir.to_path_buf(),
            last_used,
        })
    }
}

/// Profile directories of a target: `<target>/<profile>` and, for cross builds and tools
/// with their own target dir inside this one, `<target>/<dir>/<profile>`.
pub fn profiles(target: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for child in real_subdirs(target) {
        if is_profile(&child) {
            found.push(child);
        } else {
            found.extend(
                real_subdirs(&child)
                    .into_iter()
                    .filter(|dir| is_profile(dir)),
            );
        }
    }
    found
}

fn is_profile(dir: &Path) -> bool {
    is_dir(&dir.join(".fingerprint")) && is_dir(&dir.join("deps"))
}

/// Build output nothing current depends on: what trimming a target deletes.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Garbage {
    /// Everything to delete, each inside a profile's `.fingerprint/`, `build/`, `deps/`,
    /// `examples/` or `incremental/`. Lock files and uplifted outputs are never included.
    pub paths: Vec<PathBuf>,
    pub units: usize,
    pub incremental_dirs: usize,
}

/// How long superseded build units are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retention {
    /// Top-level units compiled this recently are current, with everything they depend on, so
    /// switching back to the previous branch soon after rebuilds nothing.
    pub grace: Duration,
    /// An older generation of a top-level unit that differs from the newest only in ways the
    /// fingerprint does not show stays current this long. `cargo clippy` and `cargo check`
    /// builds of a workspace crate, or `-p` and `--workspace` builds unifying features
    /// differently, look exactly like that, and both stay in use.
    pub variants: Duration,
}

/// Finds the garbage in `target`.
///
/// Reads only the fingerprint JSON and hash files, and lists directories; writes nothing.
pub fn garbage(target: &Path, retention: Retention, now: SystemTime) -> io::Result<Garbage> {
    let live_since = before(now, retention.grace);
    let profiles = profiles(target);
    let mut graph = Graph::default();
    for (index, profile) in profiles.iter().enumerate() {
        graph.load(index, profile)?;
    }
    let live = graph.live(retention, now);

    let mut garbage = Garbage::default();
    for (index, profile) in profiles.iter().enumerate() {
        let dead: HashMap<&str, &str> = graph
            .dirs
            .iter()
            .filter(|dir| dir.profile == index && dir.is_dead(&graph.nodes, &live, live_since))
            .map(|dir| (dir.hash(), dir.name.as_str()))
            .collect();
        garbage.units += dead.len();
        for name in dead.values() {
            garbage.paths.push(profile.join(".fingerprint").join(name));
            let build = profile.join("build").join(name);
            if fs::symlink_metadata(&build).is_ok() {
                garbage.paths.push(build);
            }
        }
        for sub in ["deps", "examples"] {
            for entry in entries(&profile.join(sub)) {
                let Some(name) = entry.file_name().and_then(|name| name.to_str()) else {
                    continue;
                };
                // `lib<crate>-<hash>.rlib`, `<crate>-<hash>.<cgu>.rcgu.o`, `<crate>-<hash>.dSYM`
                let stem = name.split('.').next().unwrap_or(name);
                if stem
                    .rsplit_once('-')
                    .is_some_and(|(_, hash)| dead.contains_key(hash))
                {
                    garbage.paths.push(entry);
                }
            }
        }

        let live_crates = graph.live_crates(index, &live);
        for (dir, used) in dead_incremental_dirs(&profile.join("incremental"), &live_crates) {
            if used < live_since {
                garbage.incremental_dirs += 1;
                garbage.paths.push(dir);
            }
        }
    }
    Ok(garbage)
}

/// rustc keeps one incremental directory per compiled unit of a local crate. Keeps the newest
/// ones of each crate, as many as it has live units and at least one, and returns the others
/// with the time they were last written.
fn dead_incremental_dirs(
    incremental: &Path,
    live_crates: &HashMap<&str, usize>,
) -> Vec<(PathBuf, SystemTime)> {
    let mut by_crate: BTreeMap<String, Vec<(SystemTime, PathBuf)>> = BTreeMap::new();
    for dir in real_subdirs(incremental) {
        let Some(krate) = dir
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.rsplit_once('-'))
            .map(|(krate, _)| krate.to_owned())
        else {
            continue;
        };
        let used = entries(&dir)
            .iter()
            .chain([&dir])
            .filter_map(|path| mtime(path))
            .max();
        if let Some(used) = used {
            by_crate.entry(krate).or_default().push((used, dir));
        }
    }
    let mut dead = Vec::new();
    for (krate, mut dirs) in by_crate {
        dirs.sort_by_key(|(used, _)| std::cmp::Reverse(*used));
        let keep = live_crates.get(krate.as_str()).copied().unwrap_or(0).max(1);
        dead.extend(dirs.into_iter().skip(keep).map(|(used, dir)| (dir, used)));
    }
    dead
}

/// The parts of `<role>.json` that identify a unit and its dependencies.
#[derive(Deserialize)]
struct FingerprintJson {
    rustc: u64,
    features: String,
    target: u64,
    profile: u64,
    path: u64,
    compile_kind: u64,
    #[serde(default)]
    config: u64,
    #[serde(default)]
    rustflags: Vec<String>,
    /// `[package hash, extern crate name, public, fingerprint]`.
    deps: Vec<(u64, String, bool, u64)>,
}

/// Units with the same key are generations of the same thing: same role (`lib-serde`,
/// `test-lib-api`, `bin-neoctl`, …), target, profile (which tells `check` from `build`), source
/// path and compile kind.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UnitKey {
    role: String,
    target: u64,
    profile: u64,
    path: u64,
    compile_kind: u64,
}

/// One role of a unit.
struct Node {
    dir: usize,
    key: UnitKey,
    /// Everything the JSON records about the unit itself. With its dependencies' shapes, it
    /// tells an out-of-date generation from a variant built differently in ways the
    /// fingerprint does not record. A registry crate's version shows in its source path.
    own_shape: u64,
    /// The value dependents record for it: the role's hash file, read as little-endian.
    fingerprint: u64,
    crate_name: String,
    /// The hash file is rewritten whenever the unit is compiled.
    compiled: SystemTime,
    deps: Vec<(String, u64)>,
    /// Running a build script compiles nothing, so it has no incremental directory.
    compiles: bool,
}

/// A `.fingerprint/<package>-<hash>` directory. Its hash also names the unit's files in
/// `deps/`, `build/` and `examples/`.
struct UnitDir {
    profile: usize,
    name: String,
    nodes: Vec<usize>,
    /// Some role in it could not be read or parsed, so the unit is kept.
    unknown: bool,
}

impl UnitDir {
    fn hash(&self) -> &str {
        self.name.rsplit_once('-').map_or("", |(_, hash)| hash)
    }

    fn is_dead(&self, nodes: &[Node], live: &HashSet<usize>, live_since: SystemTime) -> bool {
        !self.unknown
            && !self.nodes.is_empty()
            && self
                .nodes
                .iter()
                .all(|&node| !live.contains(&node) && nodes[node].compiled < live_since)
    }
}

#[derive(Default)]
struct Graph {
    nodes: Vec<Node>,
    dirs: Vec<UnitDir>,
}

impl Graph {
    fn load(&mut self, profile_index: usize, profile: &Path) -> io::Result<()> {
        for entry in fs::read_dir(profile.join(".fingerprint"))? {
            let entry = entry?;
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !entry.file_type()?.is_dir() || !is_unit_dir_name(&name) {
                continue;
            }
            let dir = self.dirs.len();
            let mut unit = UnitDir {
                profile: profile_index,
                name,
                nodes: Vec::new(),
                unknown: false,
            };
            for file in entries(&entry.path()) {
                let Some(role) = file
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(|name| name.strip_suffix(".json"))
                else {
                    continue;
                };
                match read_node(&file, &entry.path().join(role), role, dir) {
                    Some(node) => {
                        unit.nodes.push(self.nodes.len());
                        self.nodes.push(node);
                    }
                    None => unit.unknown = true,
                }
            }
            self.dirs.push(unit);
        }
        Ok(())
    }

    /// Nodes in use: the newest generation of every top-level unit, the top-level units
    /// `retention` keeps, and everything they depend on.
    fn live(&self, retention: Retention, now: SystemTime) -> HashSet<usize> {
        let live_since = before(now, retention.grace);
        let variants_since = before(now, retention.variants);
        let mut by_fingerprint: HashMap<u64, Vec<usize>> = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            by_fingerprint
                .entry(node.fingerprint)
                .or_default()
                .push(index);
        }
        let referenced: HashSet<usize> = self
            .nodes
            .iter()
            .flat_map(|node| &node.deps)
            .filter_map(|(_, fingerprint)| by_fingerprint.get(fingerprint))
            .flatten()
            .copied()
            .collect();
        let roots: Vec<usize> = (0..self.nodes.len())
            .filter(|index| !referenced.contains(index))
            .collect();

        let mut newest: HashMap<&UnitKey, usize> = HashMap::new();
        for &index in &roots {
            let best = newest.entry(&self.nodes[index].key).or_insert(index);
            if self.nodes[*best].compiled < self.nodes[index].compiled {
                *best = index;
            }
        }
        let shapes = self.shapes(&by_fingerprint);
        let mut pending: Vec<usize> = roots
            .iter()
            .copied()
            .filter(|&index| {
                let node = &self.nodes[index];
                let newest = newest[&node.key];
                node.compiled >= live_since
                    || (shapes[index] == shapes[newest] && node.compiled >= variants_since)
            })
            .collect();
        pending.extend(newest.into_values());

        let mut live = HashSet::new();
        let mut unresolved: HashSet<&str> = HashSet::new();
        while let Some(index) = pending.pop() {
            if !live.insert(index) {
                continue;
            }
            for (name, fingerprint) in &self.nodes[index].deps {
                match by_fingerprint.get(fingerprint) {
                    Some(deps) => pending.extend(deps),
                    // Built against a version of this dependency that has been rebuilt since,
                    // so the unit is dirty, and which generation its rebuild will use is
                    // unknown: keep them all.
                    None => {
                        unresolved.insert(name);
                    }
                }
            }
        }
        for (index, node) in self.nodes.iter().enumerate() {
            if unresolved.contains(node.crate_name.as_str()) {
                live.insert(index);
            }
        }
        live
    }

    /// For one profile, how many live compiled units each crate has.
    fn live_crates(&self, profile: usize, live: &HashSet<usize>) -> HashMap<&str, usize> {
        let mut counts = HashMap::new();
        for &index in live {
            let node = &self.nodes[index];
            if node.compiles && self.dirs[node.dir].profile == profile {
                *counts.entry(node.crate_name.as_str()).or_default() += 1;
            }
        }
        counts
    }

    /// Each node's own shape combined with its dependencies' shapes, recursively.
    fn shapes(&self, by_fingerprint: &HashMap<u64, Vec<usize>>) -> Vec<u64> {
        fn visit(
            graph: &Graph,
            by_fingerprint: &HashMap<u64, Vec<usize>>,
            index: usize,
            memo: &mut Vec<Option<u64>>,
            visiting: &mut Vec<bool>,
        ) -> u64 {
            if let Some(shape) = memo[index] {
                return shape;
            }
            let node = &graph.nodes[index];
            visiting[index] = true;
            let mut deps: Vec<(&str, u64)> = Vec::new();
            for (name, fingerprint) in &node.deps {
                // A fingerprint is unique in practice; `visiting` only guards against a
                // corrupt one pointing back up the graph.
                let resolved = by_fingerprint
                    .get(fingerprint)
                    .and_then(|found| found.iter().copied().find(|&dep| !visiting[dep]));
                let shape = match resolved {
                    Some(dep) => visit(graph, by_fingerprint, dep, memo, visiting),
                    None => *fingerprint,
                };
                deps.push((name, shape));
            }
            deps.sort_unstable();
            visiting[index] = false;
            let mut hasher = DefaultHasher::new();
            (node.own_shape, deps).hash(&mut hasher);
            let shape = hasher.finish();
            memo[index] = Some(shape);
            shape
        }

        let mut memo = vec![None; self.nodes.len()];
        let mut visiting = vec![false; self.nodes.len()];
        (0..self.nodes.len())
            .map(|index| visit(self, by_fingerprint, index, &mut memo, &mut visiting))
            .collect()
    }
}

/// Reads one role of a unit. `None` when anything is missing or unexpected, so the unit is
/// kept rather than guessed at.
fn read_node(json: &Path, hash_file: &Path, role: &str, dir: usize) -> Option<Node> {
    let parsed: FingerprintJson = serde_json::from_slice(&fs::read(json).ok()?).ok()?;
    let hash = decode_hex(fs::read_to_string(hash_file).ok()?.trim())?;
    let fingerprint = u64::from_le_bytes(hash.try_into().ok()?);
    let compiled = mtime(hash_file)?;
    // Crate names have no `-`, so a role ends with its crate name, except for build scripts.
    let crate_name = if role.ends_with("build-script-build") {
        "build_script_build"
    } else {
        role.rsplit_once('-')?.1
    };
    let mut own_shape = DefaultHasher::new();
    (
        role,
        parsed.rustc,
        &parsed.features,
        parsed.target,
        parsed.profile,
        parsed.path,
        parsed.compile_kind,
        parsed.config,
        &parsed.rustflags,
    )
        .hash(&mut own_shape);
    Some(Node {
        dir,
        key: UnitKey {
            role: role.to_owned(),
            target: parsed.target,
            profile: parsed.profile,
            path: parsed.path,
            compile_kind: parsed.compile_kind,
        },
        own_shape: own_shape.finish(),
        fingerprint,
        crate_name: crate_name.to_owned(),
        compiled,
        deps: parsed
            .deps
            .into_iter()
            .map(|(_, name, _, fingerprint)| (name, fingerprint))
            .collect(),
        compiles: !role.starts_with("run-"),
    })
}

fn before(now: SystemTime, duration: Duration) -> SystemTime {
    now.checked_sub(duration).unwrap_or(SystemTime::UNIX_EPOCH)
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// `<package>-<16 hex>`.
fn is_unit_dir_name(name: &str) -> bool {
    name.rsplit_once('-').is_some_and(|(package, hash)| {
        !package.is_empty()
            && hash.len() == 16
            && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Subdirectories, not following symlinks, skipping hidden ones (cendrillon's own trash).
fn real_subdirs(dir: &Path) -> Vec<PathBuf> {
    entries(dir)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !name.starts_with('.'))
                && is_dir(path)
        })
        .collect()
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|entry| Some(entry.ok()?.path()))
        .collect()
}

fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_file())
}

fn mtime(path: &Path) -> Option<SystemTime> {
    fs::symlink_metadata(path).ok()?.modified().ok()
}

#[cfg(test)]
pub mod fixtures {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A workspace with an `app` binary depending on a `dep` library, built by real Cargo.
    pub struct Workspace {
        pub root: PathBuf,
    }

    impl Workspace {
        pub fn new(dir: &Path) -> Self {
            let root = dir.join("ws");
            for (name, manifest, source, file) in [
                (
                    "dep",
                    "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                     [features]\nextra = []\n",
                    "pub fn value() -> u32 { 1 }\n",
                    "lib.rs",
                ),
                (
                    "app",
                    "[package]\nname = \"app\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n\
                     [dependencies]\ndep = { path = \"../dep\" }\n",
                    "fn main() { println!(\"{}\", dep::value()); }\n",
                    "main.rs",
                ),
            ] {
                fs::create_dir_all(root.join(name).join("src")).unwrap();
                fs::write(root.join(name).join("Cargo.toml"), manifest).unwrap();
                fs::write(root.join(name).join("src").join(file), source).unwrap();
            }
            fs::write(
                root.join("Cargo.toml"),
                "[workspace]\nmembers = [\"app\", \"dep\"]\nresolver = \"3\"\n",
            )
            .unwrap();
            Self { root }
        }

        pub fn target(&self) -> PathBuf {
            self.root.join("target")
        }

        /// Enables `dep/extra` from `app`, which makes new generations of both crates.
        pub fn enable_extra(&self) {
            let manifest = self.root.join("app/Cargo.toml");
            let text = fs::read_to_string(&manifest).unwrap().replace(
                "dep = { path = \"../dep\" }",
                "dep = { path = \"../dep\", features = [\"extra\"] }",
            );
            fs::write(manifest, text).unwrap();
        }

        /// Runs a Cargo command and returns its `-v` output, which says what was Fresh.
        pub fn cargo(&self, command: &str) -> String {
            self.cargo_with(command, &[])
        }

        pub fn cargo_with(&self, command: &str, env: &[(&str, &str)]) -> String {
            let output = Command::new("cargo")
                .args([command, "--offline", "-v"])
                .current_dir(&self.root)
                .env_remove("CARGO_TARGET_DIR")
                .env_remove("RUSTFLAGS")
                .env_remove("RUSTC_WORKSPACE_WRAPPER")
                .env("CARGO_INCREMENTAL", "1")
                .envs(env.iter().copied())
                .output()
                .unwrap();
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            assert!(output.status.success(), "cargo {command} failed:\n{stderr}");
            stderr
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::Workspace;
    use super::*;

    const DAY: Duration = Duration::from_secs(24 * 3600);

    fn keep(grace: Duration, variants: Duration) -> Retention {
        Retention { grace, variants }
    }

    /// Just after everything built so far.
    fn later() -> SystemTime {
        SystemTime::now() + Duration::from_secs(1)
    }

    fn fingerprint_dirs(target: &Path, package: &str) -> usize {
        entries(&target.join("debug/.fingerprint"))
            .iter()
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.rsplit_once('-').is_some_and(|(p, _)| p == package))
            })
            .count()
    }

    #[test]
    fn recognizes_a_target_and_nothing_that_merely_resembles_one() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        let target = Target::classify(&ws.target()).unwrap();
        assert_eq!(profiles(&target.root), [ws.target().join("debug")]);
        assert!(target.last_used.elapsed().unwrap() < Duration::from_secs(600));

        let fake = dir.path().join("fake");
        fs::create_dir_all(fake.join("debug/.fingerprint")).unwrap();
        fs::create_dir_all(fake.join("debug/deps")).unwrap();
        assert_eq!(Target::classify(&fake), None);
        fs::write(fake.join(".rustc_info.json"), "{}").unwrap();
        assert!(Target::classify(&fake).is_some());
        fs::remove_dir_all(fake.join("debug/deps")).unwrap();
        assert_eq!(Target::classify(&fake), None);
    }

    #[test]
    fn outdated_generations_are_garbage_and_the_next_builds_are_fresh() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        ws.cargo("check");
        let none = garbage(&ws.target(), keep(Duration::ZERO, Duration::ZERO), later());
        assert_eq!(none.unwrap(), Garbage::default());

        ws.enable_extra();
        ws.cargo("build");
        ws.cargo("check");
        assert_eq!(fingerprint_dirs(&ws.target(), "dep"), 4);

        // Everything was compiled just now, so the grace period protects it all.
        let protected = garbage(&ws.target(), keep(DAY, DAY), SystemTime::now());
        assert_eq!(protected.unwrap().units, 0);

        // After it: the first build's and the first check's app and dep are out of date,
        // whatever `variants` says, since `dep` is built with other features.
        let found = garbage(&ws.target(), keep(Duration::ZERO, 7 * DAY), later()).unwrap();
        assert_eq!(found.units, 4, "{found:#?}");
        assert_eq!(found.incremental_dirs, 4, "{found:#?}");
        let debug = ws.target().join("debug");
        assert!(found.paths.iter().all(|path| path.starts_with(&debug)));
        for path in &found.paths {
            fs::remove_dir_all(path)
                .or_else(|_| fs::remove_file(path))
                .unwrap();
        }

        let build = ws.cargo("build");
        assert!(!build.contains("Compiling"), "{build}");
        let check = ws.cargo("check");
        assert!(!check.contains("Checking"), "{check}");
    }

    #[test]
    fn variants_that_look_the_same_stay_current_for_a_while() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("check");
        // As `cargo clippy` does: same fingerprint, other unit directories.
        ws.cargo_with("check", &[("RUSTC_WORKSPACE_WRAPPER", "/usr/bin/env")]);
        assert_eq!(fingerprint_dirs(&ws.target(), "dep"), 2);

        let found = garbage(&ws.target(), keep(Duration::ZERO, 7 * DAY), later()).unwrap();
        assert_eq!(found.units, 0, "{found:#?}");
        let found = garbage(&ws.target(), keep(Duration::ZERO, Duration::ZERO), later());
        assert_eq!(found.unwrap().units, 2);
    }

    #[test]
    fn unreadable_units_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        ws.enable_extra();
        ws.cargo("build");
        for path in entries(&ws.target().join("debug/.fingerprint")) {
            for file in entries(&path) {
                if file.extension().is_some_and(|ext| ext == "json") {
                    fs::write(&file, "not json").unwrap();
                }
            }
        }
        let found = garbage(&ws.target(), keep(Duration::ZERO, Duration::ZERO), later());
        assert_eq!(found.unwrap().units, 0);
    }

    #[test]
    fn decodes_fingerprints_as_cargo_writes_them() {
        assert_eq!(
            u64::from_le_bytes(decode_hex("7c1e9a5aa56ea158").unwrap().try_into().unwrap()),
            6386507403056782972
        );
        assert_eq!(decode_hex("abc"), None);
        assert!(is_unit_dir_name("alass-core-67efb9fd379c5457"));
        assert!(!is_unit_dir_name("notes-2026"));
    }
}
