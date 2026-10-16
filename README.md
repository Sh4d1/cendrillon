# cendrillon

A macOS background daemon that deletes the Bazel caches and Cargo build output you no longer
use, before they fill your disk.

Bazel never deletes an output base on its own. Each git worktree gets its own one, often tens
of gigabytes, and it stays on disk after the worktree is gone. The disk cache and the
repository cache only shrink if you configure a garbage collector. Cargo never deletes old
build units either: every change of features, flags, toolchain or dependency versions adds
new ones next to the old. On a large multi-language repository this adds up to hundreds of
gigabytes. cendrillon runs at login as a launchd agent and cleans up continuously.

## Install

```sh
cargo install --git https://github.com/ae2rs/cendrillon
cendrillon install      # start now and at every login
```

Before installing, see what it finds and what a pass would delete:

```sh
cendrillon status                 # every cache and target found, its size and last use
cendrillon clean --dry-run        # what a full pass would delete right now
cendrillon trim --dry-run target  # what trimming one Cargo target would delete
```

`cendrillon uninstall` stops the agent and removes it. Logs go to
`~/Library/Logs/cendrillon.log`.

## What it deletes

| What | When |
| --- | --- |
| Output base whose workspace no longer exists (a deleted worktree) | Within a minute |
| Output base with no Bazel command for 24 hours | Hourly sweep |
| Disk cache (`--disk_cache`) entries not hit for 72 hours | Hourly sweep |
| Repository cache downloads not used for 72 hours | Hourly sweep |
| Install base (an extracted Bazel version) unused for 72 hours | Hourly sweep |
| Cargo build units nothing current depends on (see below) | Hourly sweep |
| Cargo target directory nothing was built in for 7 days | Hourly sweep |

When free space drops below 15%, idle output bases and disk-cache entries on that disk are
deleted oldest first until 25% of the disk is free, without waiting for the deadlines
above. Nothing used in the last 2 hours is touched. Pressure rounds run at most every 10
minutes. A round that does not raise free space pauses them for an hour, for example
when local Time Machine snapshots still hold the deleted files, so cendrillon never
deletes more and more without effect.

Everything it deletes, Bazel or Cargo can rebuild or download again. The repository contents
cache (`contents/`) is left to Bazel's own `--repo_contents_cache_gc_*` settings, because live
external repositories point into it.

### Cargo targets

cendrillon rebuilds Cargo's unit graph from `.fingerprint/<unit>/<role>.json`, where every
unit lists the fingerprints of the units it was built against. It keeps:

- every unit Cargo compiled or read in the last 24 hours;
- the most recently used generation of every top-level unit (each binary, test, example,
  check), and the one the last build uplifted to `target/<profile>/`;
- everything those depend on.

A Fresh build writes nothing, but Cargo reads the fingerprint hash file of every unit in
the build, and macOS records that read in the file's access time as long as the access time
is older than the modification time. On each hourly pass cendrillon notes which hash files
were read, in `<target>/.cendrillon-reads.json`, then sets their access times back to just
before their modification times so the next read shows too. Cargo only looks at contents
and modification times, which never change.

Incremental-compilation directories follow their crate's units. A unit cendrillon cannot
read stops the trim of its whole target.

Limits: generations from an old `[profile]` setting keep their newest unit until the target
goes idle or the disk fills up. A target copied with APFS clones (for example into a new
worktree) shares its blocks with the copy, so trimming one frees space only once the copy is
trimmed too.

## Where it looks

- Bazel's default output root, `/private/var/tmp/_bazel_$USER`, plus `~/Library/Caches/bazel`
  and `~/.cache/bazel`.
- Paths given to `--output_user_root`, `--output_base`, `--install_base`, `--disk_cache` and
  `--repository_cache` in `~/.bazelrc`, `/etc/bazel.bazelrc`, and the `.bazelrc` of every
  workspace it finds, following `import` and `try-import`.
- Once a day, a walk of your home directory. It skips the folders macOS protects (Desktop,
  Documents, Downloads, Pictures, Movies, Music, Public, and `~/Library` except
  `~/Library/Caches`) without even opening them, so it needs no Full Disk Access and
  triggers no permission prompt. It skips `.git`, `node_modules`, `.Trash` and `.rustup`,
  and does not cross into other volumes.

Recognition is strict, because anything recognized may be deleted:

- An output base must contain both `DO_NOT_BUILD_HERE` and `README`, naming the same
  absolute workspace path.
- A disk cache must have only two-hex-digit shard directories under `ac/` and `cas/`.
- An install base must sit in an `install/` directory and contain `build-label.txt`.
- A Cargo target must contain `.rustc_info.json` and a profile directory with both
  `.fingerprint/` and `deps/`.

A workspace counts as deleted only if its parent directory still exists and it is not on an
unmounted volume under `/Volumes`.

## How it stays out of Bazel's and Cargo's way

- **Output bases.** The Bazel client holds an `fcntl` lock on `<output_base>/lock` for the
  whole of every command. cendrillon takes the same lock, so it never deletes an output base
  a command is using, and no command can start while it works. It then stops the base's
  Bazel server, if one is running and its command line names this output base. It renames
  the base to a hidden `.cendrillon-trash-*` sibling and deletes that. A deletion cut short
  is finished on the next pass, and Bazel never sees a half-deleted output base.
- **Changes after planning.** If Bazel used an output base or install base after
  cendrillon decided to delete it, the item is kept.
- **Symlinks.** Output bases link into your source tree. Deletion never follows a symlink.
- **Disk cache.** Eviction works like Bazel's own garbage collector. It holds `gc/lock`, so
  it never runs alongside Bazel's collector. It skips `tmp/`, evicts `ac/` before `cas/`,
  and reads each entry's last use just before deleting it. Running builds are safe.
- **Install bases.** It follows Bazel's install-base collector: it takes the
  `<install_base>.lock` that running Bazel servers hold, renames the install base, then
  deletes it.
- **Cargo targets.** Cargo holds `flock` locks on `.cargo-build-lock` and `.cargo-lock` in
  every profile directory for the whole of a build. cendrillon takes both, exclusively, and
  skips the target if any is held. Under the locks it reads the unit graph and renames the
  garbage into a `.cendrillon-trash-*` directory inside the target. Then it releases the
  locks and deletes the trash, so builds wait seconds, not minutes. Nothing is ever listed
  or deleted through a symlink.

## Configuration

Settings live in `~/.config/cendrillon/config.toml`, and every key is optional. Durations use
[humantime](https://docs.rs/humantime) syntax. The defaults:

```toml
dry_run = false               # log what would be deleted instead of deleting it
output_base_idle = "24h"
cache_entry_idle = "72h"
install_base_idle = "72h"
unit_idle = "24h"             # delete Cargo units Cargo neither compiled nor read this long
target_idle = "7d"            # delete a Cargo target Cargo did not use for this long
pressure_free_percent = 15    # below this much free space, evict early...
target_free_percent = 25      # ...until this much is free
recent_use_floor = "2h"       # never evict anything used more recently
tick_interval = "60s"         # deleted-workspace and disk-pressure checks
sweep_interval = "1h"         # idle deadlines
pressure_cooldown = "10m"
scan_home = true
home_scan_interval = "24h"
extra_roots = []              # more directories to search for caches and targets
```

The daemon reads its config at startup. After editing the file, run `cendrillon install` to
restart it.

## License

MIT
