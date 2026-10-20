//! Replaying the macOS file system events journal: which directories changed since an event,
//! without listing a single one. `fseventsd` records changes per volume and keeps them across
//! reboots.

use std::collections::BTreeSet;
use std::ffi::{CStr, OsStr, c_char, c_void};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use core_foundation_sys::array::{CFArrayCreate, CFArrayRef, kCFTypeArrayCallBacks};
use core_foundation_sys::base::{
    CFAllocatorRef, CFIndex, CFRelease, CFTypeRef, kCFAllocatorDefault,
};
use core_foundation_sys::runloop::{
    CFRunLoopGetCurrent, CFRunLoopRef, CFRunLoopRunInMode, CFRunLoopStop, kCFRunLoopDefaultMode,
};
use core_foundation_sys::string::{CFStringCreateWithBytes, CFStringRef, kCFStringEncodingUTF8};
use core_foundation_sys::uuid::{CFUUIDGetUUIDBytes, CFUUIDRef};
use nix::libc::dev_t;

type StreamRef = *mut c_void;
type Callback = extern "C" fn(StreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[repr(C)]
struct StreamContext {
    version: CFIndex,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

const CREATE_FLAG_NO_DEFER: u32 = 0x2;
const EVENT_FLAG_MUST_SCAN_SUB_DIRS: u32 = 0x1;
const EVENT_FLAG_EVENT_IDS_WRAPPED: u32 = 0x8;
const EVENT_FLAG_HISTORY_DONE: u32 = 0x10;
/// `FSEventStreamSetExclusionPaths` refuses more.
const MAX_EXCLUSIONS: usize = 8;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: CFAllocatorRef,
        callback: Callback,
        context: *const StreamContext,
        paths: CFArrayRef,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> StreamRef;
    fn FSEventStreamSetExclusionPaths(stream: StreamRef, paths: CFArrayRef) -> u8;
    fn FSEventStreamScheduleWithRunLoop(
        stream: StreamRef,
        run_loop: CFRunLoopRef,
        mode: CFStringRef,
    );
    fn FSEventStreamStart(stream: StreamRef) -> u8;
    fn FSEventStreamStop(stream: StreamRef);
    fn FSEventStreamInvalidate(stream: StreamRef);
    fn FSEventStreamRelease(stream: StreamRef);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsCopyUUIDForDevice(device: dev_t) -> CFUUIDRef;
}

/// What changed under the watched directories.
#[derive(Debug, Default)]
pub struct Changes {
    /// Directories with an entry created, modified, renamed or removed.
    pub dirs: BTreeSet<PathBuf>,
    /// Directories whose changes the journal lost: anything under them may have changed.
    pub rescan: BTreeSet<PathBuf>,
    /// The ids restarted from zero, so nothing older can be told apart from newer.
    pub ids_wrapped: bool,
    /// The last event replayed, where the next replay starts.
    pub last_event: u64,
}

/// The latest event id on any volume. Every later change gets a greater id.
pub fn current_event() -> u64 {
    // SAFETY: no arguments, no preconditions.
    unsafe { FSEventsGetCurrentEventId() }
}

/// Names the journal of the volume holding `path`. It changes when macOS resets the journal,
/// which invalidates every event id saved before. `None` when the volume keeps no journal.
pub fn volume(path: &Path) -> Option<String> {
    let device = dev_t::try_from(std::fs::metadata(path).ok()?.dev()).ok()?;
    // SAFETY: returns an owned CFUUID or null.
    let uuid = unsafe { FSEventsCopyUUIDForDevice(device) };
    if uuid.is_null() {
        return None;
    }
    let uuid = Owned(uuid.cast());
    // SAFETY: `uuid` is a valid CFUUID, and `CFUUIDBytes` is sixteen `u8` fields, `repr(C)`.
    let bytes: [u8; 16] = unsafe { std::mem::transmute(CFUUIDGetUUIDBytes(uuid.0.cast())) };
    Some(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The changes under `watched` recorded after event `since`, leaving out those under
/// `excluded` (only the first few are honoured). Events reach the journal a little late, up to
/// a minute: the most recent changes may only show in the next replay, which `last_event`
/// starts after. Stops after `timeout`, with what it has replayed so far.
pub fn replay(
    watched: &[PathBuf],
    excluded: &[PathBuf],
    since: u64,
    timeout: Duration,
) -> Result<Changes> {
    let mut sink = Sink {
        changes: Changes {
            last_event: since,
            ..Changes::default()
        },
        history_done: false,
    };
    // Until the stream is gone, `sink` is only reached through this pointer, as the callback does.
    let shared = ptr::from_mut(&mut sink);
    let context = StreamContext {
        version: 0,
        info: shared.cast(),
        retain: ptr::null(),
        release: ptr::null(),
        copy_description: ptr::null(),
    };
    let watched = path_array(watched);
    let excluded = &excluded[..excluded.len().min(MAX_EXCLUSIONS)];
    let excluded = (!excluded.is_empty()).then(|| path_array(excluded));
    // SAFETY: the arrays and context are valid for each call; `sink` outlives the stream,
    // released at the end of this block, and the callback only runs inside the run loop below.
    unsafe {
        let raw = FSEventStreamCreate(
            kCFAllocatorDefault,
            on_events,
            &context,
            watched.0.cast(),
            since,
            0.0,
            CREATE_FLAG_NO_DEFER,
        );
        if raw.is_null() {
            bail!("FSEventStreamCreate failed");
        }
        let mut stream = Stream {
            raw,
            scheduled: false,
            started: false,
        };
        if let Some(excluded) = &excluded
            && FSEventStreamSetExclusionPaths(stream.raw, excluded.0.cast()) == 0
        {
            bail!("FSEventStreamSetExclusionPaths failed");
        }
        FSEventStreamScheduleWithRunLoop(stream.raw, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
        stream.scheduled = true;
        if FSEventStreamStart(stream.raw) == 0 {
            bail!("FSEventStreamStart failed");
        }
        stream.started = true;
        let deadline = Instant::now() + timeout;
        while !(*shared).history_done && Instant::now() < deadline {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 1.0, 0);
        }
    }
    if !sink.history_done {
        tracing::warn!(
            "replaying the file system journal took over {}; resuming next time",
            humantime::format_duration(timeout)
        );
    }
    Ok(sink.changes)
}

struct Sink {
    changes: Changes,
    history_done: bool,
}

extern "C" fn on_events(
    _stream: StreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    ids: *const u64,
) {
    if count == 0 {
        return;
    }
    // SAFETY: `info` is the `Sink` given to the stream, not otherwise borrowed while the run
    // loop runs; without `kFSEventStreamCreateFlagUseCFTypes`, `paths` is a C string array, and
    // the three arrays hold `count` entries.
    let (sink, paths, flags, ids) = unsafe {
        (
            &mut *info.cast::<Sink>(),
            slice::from_raw_parts(paths.cast::<*const c_char>(), count),
            slice::from_raw_parts(flags, count),
            slice::from_raw_parts(ids, count),
        )
    };
    for ((&path, &flag), &id) in paths.iter().zip(flags).zip(ids) {
        if flag & EVENT_FLAG_HISTORY_DONE != 0 {
            sink.history_done = true;
            // SAFETY: the callback runs on the thread whose run loop the stream is scheduled on.
            unsafe { CFRunLoopStop(CFRunLoopGetCurrent()) };
            continue;
        }
        let changes = &mut sink.changes;
        changes.last_event = changes.last_event.max(id);
        changes.ids_wrapped |= flag & EVENT_FLAG_EVENT_IDS_WRAPPED != 0;
        // SAFETY: a NUL-terminated path, valid during the callback.
        let path = PathBuf::from(OsStr::from_bytes(
            unsafe { CStr::from_ptr(path) }.to_bytes(),
        ));
        if flag & EVENT_FLAG_MUST_SCAN_SUB_DIRS != 0 {
            changes.rescan.insert(path);
        } else {
            changes.dirs.insert(path);
        }
    }
}

/// A Core Foundation object, released when dropped.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: `Owned` holds the only reference this code took.
        unsafe { CFRelease(self.0) }
    }
}

struct Stream {
    raw: StreamRef,
    scheduled: bool,
    started: bool,
}

impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: a stream `replay` created, then undone in the reverse order of its steps.
        unsafe {
            if self.started {
                FSEventStreamStop(self.raw);
            }
            if self.scheduled {
                FSEventStreamInvalidate(self.raw);
            }
            FSEventStreamRelease(self.raw);
        }
    }
}

fn path_array(paths: &[PathBuf]) -> Owned {
    let strings: Vec<Owned> = paths
        .iter()
        .filter_map(|path| {
            let bytes = path.as_os_str().as_bytes();
            // SAFETY: `bytes` is valid for its length. Invalid UTF-8 gives null.
            let string = unsafe {
                CFStringCreateWithBytes(
                    kCFAllocatorDefault,
                    bytes.as_ptr(),
                    bytes.len() as CFIndex,
                    kCFStringEncodingUTF8,
                    0,
                )
            };
            (!string.is_null()).then(|| Owned(string.cast()))
        })
        .collect();
    let values: Vec<CFTypeRef> = strings.iter().map(|string| string.0).collect();
    // SAFETY: `values` holds valid CFStrings, which the array retains.
    Owned(
        unsafe {
            CFArrayCreate(
                kCFAllocatorDefault,
                values.as_ptr(),
                values.len() as CFIndex,
                &kCFTypeArrayCallBacks,
            )
        }
        .cast(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replays_directories_changed_since_an_event() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let since = current_event();
        let changed = root.join("a/b");
        std::fs::create_dir_all(&changed).unwrap();
        std::fs::write(changed.join("file"), "").unwrap();
        std::fs::create_dir_all(root.join("excluded/c")).unwrap();

        // Events reach the journal a little late.
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut seen = BTreeSet::new();
        let mut from = since;
        while !seen.contains(&changed) && Instant::now() < deadline {
            let changes = replay(
                std::slice::from_ref(&root),
                &[root.join("excluded")],
                from,
                Duration::from_secs(30),
            )
            .unwrap();
            assert!(changes.last_event >= from);
            from = changes.last_event;
            seen.extend(changes.dirs);
            std::thread::sleep(Duration::from_millis(500));
        }
        assert!(seen.contains(&changed), "saw {seen:?}");
        assert!(
            !seen
                .iter()
                .any(|path| path.starts_with(root.join("excluded")))
        );
    }

    #[test]
    fn names_the_journal_of_the_home_volume() {
        let home = std::env::var_os("HOME").unwrap();
        let volume = volume(Path::new(&home)).unwrap();
        assert_eq!(volume.len(), 32);
    }
}
