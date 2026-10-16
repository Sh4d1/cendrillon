//! Cleaning passes, and the loop that runs them in the background.

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

pub struct Cleaner<'a> {
    pub env: &'a Env,
    pub config: &'a Config,
    pub executor: Executor<'a>,
}

impl Cleaner<'_> {
    /// One pass over every cache found: plan, then delete. Returns the bytes freed.
    pub fn pass(&self, state: &State, full: bool) -> Result<u64> {
        let inventory = discovery::discover(self.env, &self.config.extra_roots, state);
        let sweep = if full {
            Sweep::Full {
                pressure: self.pressure(&inventory)?,
            }
        } else {
            Sweep::Orphans
        };
        let actions = plan(&inventory, SystemTime::now(), self.config, sweep);
        Ok(self.executor.run(&actions))
    }

    /// The disk holding the home directory, when it is short of free space.
    pub fn disk_under_pressure(&self) -> Result<Option<DiskSpace>> {
        let disk = DiskSpace::of(&self.env.home)
            .with_context(|| format!("reading free space of {}", self.env.home.display()))?;
        let threshold = f64::from(self.config.pressure_free_percent);
        Ok((disk.free_percent() < threshold).then_some(disk))
    }

    fn pressure(&self, inventory: &Inventory) -> Result<Option<Pressure>> {
        let Some(disk) = self.disk_under_pressure()? else {
            return Ok(None);
        };
        let bytes_to_free = disk.shortfall(self.config.target_free_percent);
        tracing::info!(
            "disk is {:.1}% free; freeing {} to reach {}%",
            disk.free_percent(),
            format_bytes(bytes_to_free),
            self.config.target_free_percent
        );
        Ok(Some(Pressure {
            bytes_to_free,
            usage: Usage::measure(inventory),
        }))
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
        },
    };
    let mut state = State::load(&env.state_file());
    let mut last_full: Option<Instant> = None;
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
        let full = last_full.is_none_or(|time| {
            time.elapsed() >= config.sweep_interval
                || (time.elapsed() >= config.pressure_cooldown
                    && matches!(cleaner.disk_under_pressure(), Ok(Some(_))))
        });
        match cleaner.pass(&state, full) {
            Ok(0) => {}
            Ok(freed) => tracing::info!("pass freed {}", format_bytes(freed)),
            Err(e) => tracing::warn!("pass failed: {e:#}"),
        }
        if full {
            last_full = Some(Instant::now());
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
