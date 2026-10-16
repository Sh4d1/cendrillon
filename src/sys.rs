//! Thin wrappers over the system calls the cleaner needs.

use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::fcntl::{FcntlArg, fcntl};
use nix::libc;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use walkdir::WalkDir;

/// An exclusive advisory lock, released when dropped.
pub struct FileLock {
    _file: File,
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

/// Takes, without blocking, the lock Bazel takes on its lock files: an `fcntl` record lock on
/// byte 0 (`blaze_util_posix.cc`, and Java's whole-file `FileChannel` locks, overlap it).
/// Returns `None` when another process holds it.
///
/// POSIX record locks belong to the process and are dropped when *any* descriptor of the
/// file is closed, so nothing else in this process may open a locked file.
pub fn try_lock(path: &Path) -> io::Result<Option<FileLock>> {
    let file = open_lock_file(path)?;
    let request = libc::flock {
        l_start: 0,
        l_len: 1,
        l_pid: 0,
        l_type: libc::F_WRLCK as libc::c_short,
        l_whence: libc::SEEK_SET as libc::c_short,
    };
    match fcntl(&file, FcntlArg::F_SETLK(&request)) {
        Ok(_) => Ok(Some(FileLock { _file: file })),
        Err(Errno::EAGAIN | Errno::EACCES) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Takes, without blocking, an exclusive `flock` lock: the kind Cargo takes on its
/// `.cargo-lock` files through `File::try_lock`. Returns `None` when another process holds
/// it, shared or exclusive.
pub fn try_flock(path: &Path) -> io::Result<Option<FileLock>> {
    let file = open_lock_file(path)?;
    match file.try_lock() {
        Ok(()) => Ok(Some(FileLock { _file: file })),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(e)) => Err(e),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskSpace {
    pub available: u64,
    pub total: u64,
}

impl DiskSpace {
    pub fn of(path: &Path) -> io::Result<Self> {
        let stat = nix::sys::statvfs::statvfs(path)?;
        let block = stat.fragment_size() as u64;
        Ok(Self {
            available: stat.blocks_available() as u64 * block,
            total: stat.blocks() as u64 * block,
        })
    }

    pub fn free_percent(self) -> f64 {
        if self.total == 0 {
            return 100.0;
        }
        self.available as f64 * 100.0 / self.total as f64
    }

    /// Bytes to free before `percent` of the disk is available.
    pub fn shortfall(self, percent: u8) -> u64 {
        (self.total / 100 * u64::from(percent)).saturating_sub(self.available)
    }
}

/// The pid of the Bazel server that owns `output_base`, if one is running.
///
/// The pid file can outlive its server, so the process must also name this output base on its
/// command line; otherwise a reused pid could belong to anything.
pub fn bazel_server(output_base: &Path) -> Option<Pid> {
    let text = std::fs::read_to_string(output_base.join("server/server.pid.txt")).ok()?;
    let pid = Pid::from_raw(text.trim().parse().ok()?);
    kill(pid, None).ok()?;
    let ps = Command::new("/bin/ps")
        .args(["-ww", "-o", "command=", "-p"])
        .arg(pid.to_string())
        .output()
        .ok()?;
    let command = String::from_utf8_lossy(&ps.stdout);
    // The value runs to the next flag, so paths with spaces survive.
    let (_, rest) = command.split_once("--output_base=")?;
    let named = Path::new(rest.split(" --").next()?.trim_end());
    let same = named == output_base
        || std::fs::canonicalize(named).ok()? == std::fs::canonicalize(output_base).ok()?;
    same.then_some(pid)
}

/// Stops a process: SIGTERM, then SIGKILL if it is still alive after `grace`.
pub fn terminate(pid: Pid, grace: Duration) -> io::Result<()> {
    for signal in [Signal::SIGTERM, Signal::SIGKILL] {
        match kill(pid, signal) {
            Ok(()) => {}
            Err(Errno::ESRCH) => return Ok(()),
            Err(e) => return Err(e.into()),
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if kill(pid, None) == Err(Errno::ESRCH) {
                return Ok(());
            }
            sleep(Duration::from_millis(100));
        }
    }
    Err(io::Error::other(format!("pid {pid} survived SIGKILL")))
}

/// Bytes allocated on disk by everything under `path`, counting each hard-linked file once.
pub fn tree_size(path: &Path) -> u64 {
    let mut seen = HashSet::new();
    WalkDir::new(path)
        .follow_root_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter_map(|entry| entry.metadata().ok())
        .map(|meta| allocated(&meta, &mut seen))
        .sum()
}

/// Deletes `path` and everything under it, first making read-only directories writable, as
/// Bazel leaves many of them. Symlinks are removed, never followed. Returns the bytes
/// allocated by the tree.
pub fn remove_tree(path: &Path) -> io::Result<u64> {
    let mut seen = HashSet::new();
    let mut size = 0;
    // Pre-order, so a directory is made readable before the walk lists it.
    let entries = WalkDir::new(path).follow_root_links(false);
    for entry in entries.into_iter().filter_map(Result::ok) {
        let Ok(meta) = entry.metadata() else { continue };
        size += allocated(&meta, &mut seen);
        let mode = meta.permissions().mode();
        if meta.is_dir() && mode & 0o700 != 0o700 {
            std::fs::set_permissions(entry.path(), PermissionsExt::from_mode(mode | 0o700))?;
        }
    }
    std::fs::remove_dir_all(path)?;
    Ok(size)
}

/// Allocated bytes of one file. `seen` deduplicates hard links.
pub fn allocated(meta: &std::fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
    if meta.nlink() > 1 && !meta.is_dir() && !seen.insert((meta.dev(), meta.ino())) {
        return 0;
    }
    meta.blocks() * 512
}

#[cfg(test)]
pub mod testing {
    use std::io::{BufRead, BufReader};
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    /// Holds a lock on a file from another process, as a Bazel client or Cargo would, until
    /// dropped.
    pub struct LockHolder(Child);

    impl LockHolder {
        /// Bazel's `fcntl` record lock on byte 0.
        pub fn fcntl(path: &Path) -> Self {
            // struct flock on macOS: off_t l_start, off_t l_len, pid_t l_pid, short l_type,
            // short l_whence.
            Self::spawn(
                path,
                r#"my $request = pack("q q l s s", 0, 1, 0, F_WRLCK, 0);
                   fcntl($f, F_SETLK, $request) or die "lock: $!";"#,
            )
        }

        /// Cargo's shared `flock`, as on `.cargo-lock` during a build.
        pub fn flock_shared(path: &Path) -> Self {
            Self::spawn(path, r#"flock($f, LOCK_SH | LOCK_NB) or die "lock: $!";"#)
        }

        fn spawn(path: &Path, lock: &str) -> Self {
            let script = format!(
                r#"use Fcntl qw(:DEFAULT :flock);
                open(my $f, "+<", $ARGV[0]) or die "open: $!";
                {lock}
                $| = 1; print "locked\n"; <STDIN>;"#
            );
            let mut child = Command::new("perl")
                .args(["-e", &script])
                .arg(path)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            let mut line = String::new();
            BufReader::new(child.stdout.as_mut().unwrap())
                .read_line(&mut line)
                .unwrap();
            assert_eq!(line, "locked\n");
            Self(child)
        }
    }

    impl Drop for LockHolder {
        fn drop(&mut self) {
            drop(self.0.stdin.take());
            let _ = self.0.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::LockHolder;
    use super::*;

    #[test]
    fn lock_is_refused_while_another_process_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        std::fs::write(&path, "").unwrap();

        let holder = LockHolder::fcntl(&path);
        assert!(try_lock(&path).unwrap().is_none());

        drop(holder);
        assert!(try_lock(&path).unwrap().is_some());
    }

    #[test]
    fn flock_is_refused_while_cargo_holds_it_shared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".cargo-lock");
        std::fs::write(&path, "").unwrap();

        let holder = LockHolder::flock_shared(&path);
        assert!(try_flock(&path).unwrap().is_none());

        drop(holder);
        assert!(try_flock(&path).unwrap().is_some());
    }

    #[test]
    fn remove_tree_handles_read_only_directories() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("base");
        let nested = root.join("external/repo");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("BUILD"), vec![0u8; 10_000]).unwrap();
        std::fs::set_permissions(&nested, PermissionsExt::from_mode(0o555)).unwrap();

        let size = remove_tree(&root).unwrap();
        assert!(size >= 10_000);
        assert!(!root.exists());
    }

    #[test]
    fn remove_tree_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("BUILD"), "").unwrap();
        let root = dir.path().join("base/execroot");
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&workspace, root.join("src")).unwrap();

        remove_tree(&dir.path().join("base")).unwrap();
        assert!(workspace.join("BUILD").exists());
    }

    #[test]
    fn tree_size_counts_hard_links_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), vec![0u8; 100_000]).unwrap();
        let single = tree_size(dir.path());
        std::fs::hard_link(dir.path().join("a"), dir.path().join("b")).unwrap();
        assert_eq!(tree_size(dir.path()), single);
    }

    #[test]
    fn shortfall_is_zero_when_enough_is_free() {
        let disk = DiskSpace {
            available: 30,
            total: 100,
        };
        assert_eq!(disk.shortfall(25), 0);
        assert_eq!(disk.shortfall(40), 10);
    }

    #[test]
    fn no_server_without_pid_file() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(bazel_server(dir.path()), None);
    }
}
