# cendrillon

A macOS background daemon that deletes the Bazel caches you no longer use, before they fill
your disk.

Bazel never deletes an output base on its own. Each git worktree gets its own one, often tens
of gigabytes, and it stays on disk after the worktree is gone. The disk cache and the
repository cache only shrink if you configure a garbage collector. On a large multi-language
repository this adds up to hundreds of gigabytes. cendrillon runs at login as a launchd agent
and cleans up continuously.

## Install

```sh
cargo install --git https://github.com/ae2rs/cendrillon
cendrillon install      # start now and at every login
```

Before installing, see what it finds and what a pass would delete:

```sh
cendrillon status            # every Bazel cache found, its size and last use
cendrillon clean --dry-run   # what a full pass would delete right now
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

When free space drops below 15%, idle output bases and disk-cache entries are deleted
oldest first until 25% of the disk is free, without waiting for the deadlines above. Nothing
used in the last 2 hours is touched, and pressure eviction runs at most every 10 minutes, so
space that macOS releases late is not reclaimed twice.

Everything it deletes, Bazel can rebuild or download again. The repository contents cache
(`contents/`) is left to Bazel's own `--repo_contents_cache_gc_*` settings, because live
external repositories point into it.

## Where it looks

- Bazel's default output root, `/private/var/tmp/_bazel_$USER`, plus `~/Library/Caches/bazel`
  and `~/.cache/bazel`.
- Paths given to `--output_user_root`, `--output_base`, `--install_base`, `--disk_cache` and
  `--repository_cache` in `~/.bazelrc`, `/etc/bazel.bazelrc`, and the `.bazelrc` of every
  workspace it finds, following `import` and `try-import`.
- Once a day, a walk of your home directory. It skips the folders macOS protects (Desktop,
  Documents, Downloads, Pictures, Movies, Music, Public, and `~/Library` except
  `~/Library/Caches`), so it needs no Full Disk Access. It does not cross into other volumes.

A workspace on an unmounted volume under `/Volumes` does not count as deleted.

## How it stays out of Bazel's way

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

## Configuration

Settings live in `~/.config/cendrillon/config.toml`, and every key is optional. Durations use
[humantime](https://docs.rs/humantime) syntax. The defaults:

```toml
dry_run = false               # log what would be deleted instead of deleting it
output_base_idle = "24h"
cache_entry_idle = "72h"
install_base_idle = "72h"
pressure_free_percent = 15    # below this much free space, evict early...
target_free_percent = 25      # ...until this much is free
recent_use_floor = "2h"       # never evict anything used more recently
tick_interval = "60s"         # deleted-workspace and disk-pressure checks
sweep_interval = "1h"         # idle deadlines
pressure_cooldown = "10m"
scan_home = true
home_scan_interval = "24h"
extra_roots = []              # more directories to search for Bazel caches
```

The daemon reads its config at startup. After editing the file, run `cendrillon install` to
restart it.

## License

MIT
