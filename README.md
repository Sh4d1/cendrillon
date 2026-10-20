# cendrillon

Cinderella for your disk. A macOS background daemon that cleans up after Bazel and Cargo, since they never do.

## Why
- Bazel creates an output base for every git worktree, often tens of GB each. It stays on disk after you delete the worktree.
- Bazel's disk cache and repository cache only shrink if you set up a garbage collector. You probably didn't.
- Cargo never deletes old build units. Each change of features, flags, toolchain or dependency versions adds more.
- On a big repository this adds up to hundreds of GB of stuff you will never use again.

## Install
```sh
cargo install cendrillon
cendrillon install   # start now and at every login
```

Trust issues? Look first:
```sh
cendrillon status                 # caches and targets found, with size and last use
cendrillon clean --dry-run        # what a full pass would delete right now
cendrillon trim --dry-run target  # what trimming one Cargo target would delete
```

- Stop and remove: `cendrillon uninstall`
- Logs: `~/Library/Logs/cendrillon.log`

## What it deletes
| What | When |
| --- | --- |
| Output base of a deleted workspace | Within a minute |
| Output base with no Bazel command for 24h | Hourly sweep |
| Disk cache (`--disk_cache`) entries unused for 72h | Hourly sweep |
| Repository cache downloads unused for 72h | Hourly sweep |
| Install base (an extracted Bazel version) unused for 72h | Hourly sweep |
| Cargo build units nothing current depends on | Hourly sweep |
| Cargo target directory unused for 7 days | Hourly sweep |

Everything it deletes can be rebuilt or downloaded again.

### When the disk fills up
- Below 15% free space, it evicts idle output bases and disk-cache entries, oldest first, until 25% is free.
- It never touches anything used in the last 2 hours.
- It runs at most every 10 minutes.
- If a round frees nothing, it pauses for an hour. (Usually Time Machine local snapshots holding on to the files.)

### What it leaves alone
- Bazel's repository contents cache (`contents/`). Live external repositories point into it. Use Bazel's `--repo_contents_cache_gc_*` flags instead.

## Cargo targets
It keeps:
- every unit Cargo compiled or read in the last 24 hours
- the newest generation of each top-level unit (binary, test, example, check), and the one in `target/<profile>/`
- everything those depend on

Incremental-compilation directories follow their crate's units.

### How it knows what Cargo read
- It rebuilds the unit graph from `.fingerprint/<unit>/<role>.json`.
- A fresh build writes nothing, but it reads each unit's fingerprint hash file.
- macOS updates a file's access time on read, if the access time is older than the modification time.
- Each hour, cendrillon records which hash files were read in `<target>/.cendrillon-reads.json`. Then it sets their access times back, so the next read shows too.
- Cargo only checks contents and modification times. It never notices.

### Limits
- Units from an old `[profile]` setting keep their newest generation until the target goes idle or the disk fills up.
- APFS clones share blocks. Trimming a cloned target (say, copied into a new worktree) frees space only once the copy is trimmed too.
- A unit it cannot read stops the trim of the whole target.

## Where it looks
- Bazel's default roots: `/private/var/tmp/_bazel_$USER`, `~/Library/Caches/bazel`, `~/.cache/bazel`.
- Paths given to `--output_user_root`, `--output_base`, `--install_base`, `--disk_cache` and `--repository_cache` in:
  - `~/.bazelrc` and `/etc/bazel.bazelrc`
  - the `.bazelrc` of every workspace it finds, following `import` and `try-import`
- Your home directory, once a day. It skips:
  - folders macOS protects (Desktop, Documents, Downloads, Pictures, Movies, Music, Public, `~/Library` except `~/Library/Caches`), so no Full Disk Access and no permission prompt
  - `.git`, `node_modules`, `.Trash`, `.rustup`
  - other volumes

### What it recognizes
Anything recognized may be deleted, so it is picky:
- **Output base:** contains `DO_NOT_BUILD_HERE` and `README`, naming the same absolute workspace path.
- **Disk cache:** only two-hex-digit shard directories under `ac/` and `cas/`.
- **Install base:** sits in an `install/` directory and contains `build-label.txt`.
- **Cargo target:** contains `.rustc_info.json` and a profile directory with `.fingerprint/` and `deps/`.
- **Deleted workspace:** its parent directory still exists, and it is not on an unmounted volume under `/Volumes`. Unplugging a drive is not a deletion.

## Safety
- **Output bases:** takes the lock every Bazel command holds on `<output_base>/lock`, so it never deletes a base in use. Stops the matching Bazel server. Renames the base to a hidden `.cendrillon-trash-*` sibling, then deletes it. Bazel never sees half a base.
- **Install bases:** takes the `<install_base>.lock` running Bazel servers hold, like Bazel's own collector.
- **Disk cache:** holds `gc/lock`, like Bazel's own collector. Skips `tmp/`, evicts `ac/` before `cas/`, and rechecks each entry's last use right before deleting it.
- **Cargo targets:** takes `.cargo-build-lock` and `.cargo-lock` in every profile directory, or skips the target if a build holds them. Moves the garbage to a trash directory under the locks, then deletes it after releasing them. Builds wait seconds, not minutes.
- **Late use:** anything Bazel used after cendrillon planned to delete it is kept.
- **Symlinks:** never followed.
- **Interrupted deletions:** finished on the next pass.

## Configuration
`~/.config/cendrillon/config.toml`. Every key is optional. Durations use [humantime](https://docs.rs/humantime) syntax. The defaults:

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

After editing, run `cendrillon install` to restart the daemon.

## License
MIT
