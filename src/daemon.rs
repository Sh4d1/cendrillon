//! Cleaning passes, and the loop that runs them in the background.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result};

use crate::config::Config;
use crate::discovery::{self, State};
use crate::env::Env;
use crate::executor::{Executor, format_bytes};
use crate::inventory::{Inventory, Usage};
use crate::planner::{Pressure, Sweep, plan};
use crate::sys::DiskSpace;

/// How long to hold off disk-pressure cleanup after a round that did not free space, for
/// instance because local Time Machine snapshots still hold the deleted files.
const PRESSURE_PAUSE: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy)]
pub enum Pass {
    /// Interrupted deletions and output bases of deleted workspaces.
    Orphans,
    /// Also the idle deadlines.
    Full,
    /// Also free space on `disk`, which is nearly full.
    Pressure(DiskSpace),
}

pub struct Cleaner<'a> {
    pub env: &'a Env,
    pub config: &'a Config,
    pub executor: Executor<'a>,
}

impl Cleaner<'_> {
    /// Finds every cache, plans, then deletes. Returns the bytes freed.
    pub fn pass(&self, state: &State, pass: Pass) -> Result<u64> {
        let inventory = discovery::discover(self.env, &self.config.extra_roots, state);
        let sweep = match pass {
            Pass::Orphans => Sweep::Orphans,
            Pass::Full => Sweep::Full { pressure: None },
            Pass::Pressure(disk) => Sweep::Full {
                pressure: Some(self.pressure(&inventory, disk)?),
            },
        };
        let actions = plan(&inventory, SystemTime::now(), self.config, sweep);
        Ok(self.executor.run(&actions))
    }

    /// A full pass, freeing space too if the disk is nearly full.
    pub fn full_pass(&self, state: &State) -> Result<u64> {
        let pass = match self.disk_under_pressure()? {
            Some(disk) => Pass::Pressure(disk),
            None => Pass::Full,
        };
        self.pass(state, pass)
    }

    /// The disk holding the home directory, when it is short of free space.
    pub fn disk_under_pressure(&self) -> Result<Option<DiskSpace>> {
        let disk = DiskSpace::of(&self.env.home)
            .with_context(|| format!("reading free space of {}", self.env.home.display()))?;
        let threshold = f64::from(self.config.pressure_free_percent);
        Ok((disk.free_percent() < threshold).then_some(disk))
    }

    fn pressure(&self, inventory: &Inventory, disk: DiskSpace) -> Result<Pressure> {
        let bytes_to_free = disk.shortfall(self.config.target_free_percent);
        tracing::info!(
            "disk is {:.1}% free; freeing {} to reach {}%",
            disk.free_percent(),
            format_bytes(bytes_to_free),
            self.config.target_free_percent
        );
        let device = fs::metadata(&self.env.home)?.dev();
        let on_disk = inventory.items().filter(|item| {
            fs::symlink_metadata(item.path()).is_ok_and(|meta| meta.dev() == device)
        });
        Ok(Pressure {
            bytes_to_free,
            usage: Usage::measure(on_disk, self.config.cargo_retention()),
        })
    }
}

/// Walks the home directory and records where it found caches.
pub fn scan_home(env: &Env, state: &mut State) -> Result<()> {
    let started = Instant::now();
    state.roots = discovery::scan_home(env);
    state.last_home_scan = Some(SystemTime::now());
    tracing::info!(
        "home scan found {} cache locations in {:.0?}",
        state.roots.len(),
        started.elapsed()
    );
    state.save(&env.state_file())
}

/// Cleans until SIGTERM or SIGINT: orphaned output bases every tick, the idle deadlines
/// every sweep interval, and sooner when the disk is nearly full.
pub fn run(env: &Env, config: &Config) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&stop))?;
    }
    let cleaner = Cleaner {
        env,
        config,
        executor: Executor {
            dry_run: config.dry_run,
            stop: &stop,
            cargo: config.cargo_retention(),
        },
    };
    let mut state = State::load(&env.state_file());
    let mut last_full: Option<Instant> = None;
    let mut next_pressure_check = Instant::now();
    tracing::info!(
        "cendrillon {} started{}",
        env!("CARGO_PKG_VERSION"),
        if config.dry_run {
            " in dry-run mode"
        } else {
            ""
        }
    );

    while !stop.load(Ordering::Relaxed) {
        let home_scan_due = state
            .last_home_scan
            .is_none_or(|time| time.elapsed().unwrap_or_default() >= config.home_scan_interval);
        if config.scan_home
            && home_scan_due
            && let Err(e) = scan_home(env, &mut state)
        {
            tracing::warn!("home scan failed: {e:#}");
        }
        let started = Instant::now();
        let disk = if started >= next_pressure_check {
            cleaner.disk_under_pressure().unwrap_or_else(|e| {
                tracing::warn!("{e:#}");
                None
            })
        } else {
            None
        };
        let sweep_due = last_full.is_none_or(|time| time.elapsed() >= config.sweep_interval);
        let pass = match disk {
            Some(disk) => Pass::Pressure(disk),
            None if sweep_due => Pass::Full,
            None => Pass::Orphans,
        };
        let freed = cleaner.pass(&state, pass).unwrap_or_else(|e| {
            tracing::warn!("pass failed: {e:#}");
            0
        });
        if freed > 0 {
            tracing::info!("pass freed {}", format_bytes(freed));
        }

        if !matches!(pass, Pass::Orphans) {
            last_full = Some(started);
        }
        if let Pass::Pressure(before) = pass {
            next_pressure_check = Instant::now() + config.pressure_cooldown;
            let after = DiskSpace::of(&env.home).unwrap_or(before);
            if freed == 0 || after.available < before.available + freed / 2 {
                tracing::warn!(
                    "free space went from {} to {} after freeing {}; pausing disk-pressure \
                     cleanup for {}",
                    format_bytes(before.available),
                    format_bytes(after.available),
                    format_bytes(freed),
                    humantime::format_duration(PRESSURE_PAUSE)
                );
                next_pressure_check = Instant::now() + PRESSURE_PAUSE;
            }
        }
        sleep_unless_stopped(config.tick_interval, &stop);
    }
    tracing::info!("cendrillon stopped");
    Ok(())
}

fn sleep_unless_stopped(duration: Duration, stop: &AtomicBool) {
    let deadline = Instant::now() + duration;
    while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
        sleep(Duration::from_secs(1).min(deadline - Instant::now()));
    }
}
