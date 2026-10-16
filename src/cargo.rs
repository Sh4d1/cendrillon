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
use std::os::unix::fs::MetadataExt;
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

/// Profile directories of a target: `<target>/<profile>`, `<target>/<triple>/<profile>` for
/// cross builds, and the same inside a nested target dir such as rust-analyzer's
/// `<target>/rust-analyzer`.
pub fn profiles(target: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for child in real_subdirs(target) {
        if is_profile(&child) {
            found.push(child);
            continue;
        }
        let nested_target = is_file(&child.join(".rustc_info.json"));
        for grandchild in real_subdirs(&child) {
            if is_profile(&grandchild) {
                found.push(grandchild);
            } else if nested_target {
                found.extend(
                    real_subdirs(&grandchild)
                        .into_iter()
                        .filter(|dir| is_profile(dir)),
                );
            }
        }
    }
    found.sort();
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
    /// fingerprint does not record stays current this long: `cargo clippy` and `cargo check`
    /// builds of a workspace crate look exactly like that, and both stay in use.
    pub variants: Duration,
}

/// Finds the garbage in `target`.
///
/// Reads fingerprint files and the first bytes of uplifted outputs, and lists directories;
/// writes nothing. Fails rather than guess when a unit cannot be read, since what it depends on is unknown.
pub fn garbage(target: &Path, retention: Retention, now: SystemTime) -> io::Result<Garbage> {
    let live_since = before(now, retention.grace);
    let profiles = profiles(target);
    let mut graph = Graph::default();
    for (index, profile) in profiles.iter().enumerate() {
        graph.load(index, profile)?;
    }
    let unreadable = graph.dirs.iter().filter(|dir| dir.unknown).count();
    if unreadable > 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{unreadable} build units have fingerprints cendrillon cannot read"),
        ));
    }
    let uplifted: HashSet<usize> = profiles
        .iter()
        .enumerate()
        .flat_map(|(index, profile)| graph.uplifted(index, profile))
        .collect();
    let live = graph.live(retention, now, &uplifted);

    let mut garbage = Garbage::default();
    for (index, profile) in profiles.iter().enumerate() {
        let dead: HashMap<&str, &str> = graph
            .dirs
            .iter()
            .filter(|dir| dir.profile == index && dir.is_dead(&graph.nodes, &live, live_since))
            .map(|dir| (dir.hash(), dir.name.as_str()))
            .collect();
        garbage.units += dead.len();
        let build = profile.join("build");
        for name in dead.values() {
            garbage.paths.push(profile.join(".fingerprint").join(name));
            if is_dir(&build) && fs::symlink_metadata(build.join(name)).is_ok() {
                garbage.paths.push(build.join(name));
            }
        }
        for sub in ["deps", "examples"] {
            for entry in entries(&profile.join(sub)) {
                if unit_hash(&entry).is_some_and(|hash| dead.contains_key(hash)) {
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

/// The unit hash in an output's name: `lib<crate>-<hash>.rlib`, `<crate>-<hash>.<cgu>.rcgu.o`,
/// `<crate>-<hash>.dSYM`, `<crate>-<hash>`.
fn unit_hash(path: &Path) -> Option<&str> {
    let name = path.file_name()?.to_str()?;
    let stem = name.split('.').next()?;
    let (_, hash) = stem.rsplit_once('-')?;
    is_unit_hash(hash).then_some(hash)
}

/// rustc keeps one incremental directory per compiled unit of a local crate, named after the
/// crate but not the unit. Keeps the most recently written ones of each crate, as many as it
/// has live units and at least one, and returns the others with the time they were last
/// written. A wrong pick costs one non-incremental compile of that crate.
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
    /// The crate's name as rustc and `--extern` know it, with `_` for `-`.
    crate_name: String,
    /// The hash file is rewritten whenever the unit is compiled.
    compiled: SystemTime,
    /// `(package hash, extern crate name, fingerprint)` of each dependency.
    deps: Vec<(u64, String, u64)>,
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
    /// `retention` keeps, the units of `uplifted` directories, and everything they depend on.
    fn live(
        &self,
        retention: Retention,
        now: SystemTime,
        uplifted: &HashSet<usize>,
    ) -> HashSet<usize> {
        let live_since = before(now, retention.grace);
        let variants_since = before(now, retention.variants);
        let mut by_fingerprint: HashMap<u64, Vec<usize>> = HashMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            by_fingerprint
                .entry(node.fingerprint)
                .or_default()
                .push(index);
        }
        let mut referenced: HashSet<usize> = HashSet::new();
        let mut packages: HashMap<u64, Vec<usize>> = HashMap::new();
        for (package, _, fingerprint) in self.nodes.iter().flat_map(|node| &node.deps) {
            for &dep in by_fingerprint.get(fingerprint).into_iter().flatten() {
                referenced.insert(dep);
                packages.entry(*package).or_default().push(dep);
            }
        }
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
        pending.extend(uplifted.iter().flat_map(|&dir| &self.dirs[dir].nodes));

        let mut live = HashSet::new();
        let mut unresolved: Vec<(u64, &str)> = Vec::new();
        while let Some(index) = pending.pop() {
            if !live.insert(index) {
                continue;
            }
            for (package, name, fingerprint) in &self.nodes[index].deps {
                match by_fingerprint.get(fingerprint) {
                    Some(deps) => pending.extend(deps),
                    // Built against a version of this dependency that has been rebuilt since,
                    // so the unit is dirty, and which generation its rebuild will use is
                    // unknown: keep them all, found by package or by crate name.
                    None => unresolved.push((*package, name)),
                }
            }
        }
        for (package, name) in unresolved {
            live.extend(packages.get(&package).into_iter().flatten());
            live.extend(
                (0..self.nodes.len()).filter(|&index| self.nodes[index].crate_name == name),
            );
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

    /// Units whose outputs the last build uplifted into the profile directory, as
    /// `<profile>/<name>` and `<profile>/examples/<name>`. A Fresh build uplifts again, so
    /// they name what is in use even when another generation was compiled more recently.
    fn uplifted(&self, profile_index: usize, profile: &Path) -> Vec<usize> {
        // `app` uplifts `deps/app-<hash>`, `libapi.rlib` uplifts `deps/libapi-<hash>.rlib`.
        let split = |path: &Path| -> Option<(String, String)> {
            let name = path.file_name()?.to_str()?;
            let (stem, rest) = name.split_at(name.find('.').unwrap_or(name.len()));
            Some((stem.to_owned(), rest.to_owned()))
        };
        let mut outputs: HashMap<(String, String), PathBuf> = HashMap::new();
        for dir in [profile.to_path_buf(), profile.join("examples")] {
            for entry in entries(&dir) {
                if unit_hash(&entry).is_none() && is_file(&entry) {
                    outputs.extend(split(&entry).map(|name| (name, entry.clone())));
                }
            }
        }
        let by_hash: HashMap<&str, usize> = (0..self.dirs.len())
            .filter(|&dir| self.dirs[dir].profile == profile_index)
            .map(|dir| (self.dirs[dir].hash(), dir))
            .collect();
        let mut uplifted = Vec::new();
        for sub in ["deps", "examples"] {
            for entry in entries(&profile.join(sub)) {
                let Some(hash) = unit_hash(&entry) else {
                    continue;
                };
                let Some(&dir) = by_hash.get(hash) else {
                    continue;
                };
                let Some((stem, rest)) = split(&entry) else {
                    continue;
                };
                let Some(stem) = stem.strip_suffix(&format!("-{hash}")) else {
                    continue;
                };
                if outputs
                    .get(&(stem.to_owned(), rest))
                    .is_some_and(|output| same_contents(output, &entry))
                {
                    uplifted.push(dir);
                }
            }
        }
        uplifted
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
            for (_, name, fingerprint) in &node.deps {
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
    let crate_name = crate_name(role);
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
        crate_name,
        compiled,
        deps: parsed
            .deps
            .into_iter()
            .map(|(package, name, _, fingerprint)| (package, name, fingerprint))
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

/// The crate a role compiles: role names are `<kind>-<target name>`, and the target name with
/// `_` for `-` is the crate name.
fn crate_name(role: &str) -> String {
    ROLE_PREFIXES
        .iter()
        .find_map(|prefix| role.strip_prefix(prefix))
        .unwrap_or(role)
        .replace('-', "_")
}

const ROLE_PREFIXES: [&str; 11] = [
    "run-build-script-",
    "build-script-",
    "test-integration-test-",
    "test-example-",
    "test-bench-",
    "test-lib-",
    "test-bin-",
    "example-",
    "bench-",
    "lib-",
    "bin-",
];

/// `<package>-<16 hex>`.
fn is_unit_dir_name(name: &str) -> bool {
    name.rsplit_once('-')
        .is_some_and(|(package, hash)| !package.is_empty() && is_unit_hash(hash))
}

/// Whether `copy` is a copy of `original`: Cargo hard-links uplifted outputs on some systems
/// and clones them on macOS. Different builds differ early in the file (a Mach-O UUID, an
/// rlib's metadata hash), so the length and first 64 KiB tell them apart.
fn same_contents(copy: &Path, original: &Path) -> bool {
    let (Ok(a), Ok(b)) = (fs::symlink_metadata(copy), fs::symlink_metadata(original)) else {
        return false;
    };
    if (a.dev(), a.ino()) == (b.dev(), b.ino()) {
        return true;
    }
    if !a.is_file() || !b.is_file() || a.len() != b.len() {
        return false;
    }
    let head = |path: &Path| -> Option<Vec<u8>> {
        use std::io::Read;
        let mut buffer = Vec::new();
        fs::File::open(path)
            .ok()?
            .take(64 * 1024)
            .read_to_end(&mut buffer)
            .ok()?;
        Some(buffer)
    };
    head(copy).is_some_and(|copy| head(original) == Some(copy))
}

fn is_unit_hash(hash: &str) -> bool {
    hash.len() == 16 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
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

/// Entries of `dir`, or none if it is not a real directory: a symlink is never listed, so
/// nothing found through one is ever deleted.
fn entries(dir: &Path) -> Vec<PathBuf> {
    if !is_dir(dir) {
        return Vec::new();
    }
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

        pub fn disable_extra(&self) {
            let manifest = self.root.join("app/Cargo.toml");
            let text = fs::read_to_string(&manifest).unwrap().replace(
                "dep = { path = \"../dep\", features = [\"extra\"] }",
                "dep = { path = \"../dep\" }",
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
    fn a_fresh_build_back_on_an_older_generation_keeps_it() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        ws.enable_extra();
        ws.cargo("build");
        ws.disable_extra();
        let fresh = ws.cargo("build");
        assert!(!fresh.contains("Compiling"), "{fresh}");

        // The second build compiled last, but the uplifted `app` links to the first.
        let found = garbage(&ws.target(), keep(Duration::ZERO, Duration::ZERO), later()).unwrap();
        assert_eq!(found.units, 0, "{found:#?}");
    }

    #[test]
    fn never_deletes_through_a_symlinked_directory() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        ws.enable_extra();
        ws.cargo("build");
        let outside = dir.path().join("outside");
        for name in ["notes-1", "notes-2", "dep-0123456789abcdef"] {
            fs::create_dir_all(outside.join(name)).unwrap();
        }
        let debug = ws.target().join("debug");
        for sub in ["incremental", "examples", "build"] {
            let _ = fs::remove_dir_all(debug.join(sub));
            std::os::unix::fs::symlink(&outside, debug.join(sub)).unwrap();
        }

        let found = garbage(&ws.target(), keep(Duration::ZERO, Duration::ZERO), later()).unwrap();
        assert!(found.units > 0);
        let outside = fs::canonicalize(&outside).unwrap();
        for path in &found.paths {
            let parent = fs::canonicalize(path.parent().unwrap()).unwrap();
            assert!(!parent.starts_with(&outside), "{}", path.display());
        }
    }

    #[test]
    fn refuses_to_trim_when_a_unit_is_unreadable() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path());
        ws.cargo("build");
        ws.enable_extra();
        ws.cargo("build");
        let unit = entries(&ws.target().join("debug/.fingerprint")).remove(0);
        for file in entries(&unit) {
            if file.extension().is_some_and(|ext| ext == "json") {
                fs::write(&file, "not json").unwrap();
            }
        }
        assert!(garbage(&ws.target(), keep(Duration::ZERO, Duration::ZERO), later()).is_err());
    }

    #[test]
    fn crate_names_come_from_role_names() {
        assert_eq!(crate_name("lib-alass_core"), "alass_core");
        assert_eq!(crate_name("test-integration-test-my-test"), "my_test");
        assert_eq!(crate_name("bin-my-tool"), "my_tool");
        assert_eq!(
            crate_name("build-script-build-script-build"),
            "build_script_build"
        );
        assert_eq!(
            crate_name("run-build-script-build-script-build"),
            "build_script_build"
        );
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
