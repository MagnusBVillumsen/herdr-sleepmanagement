mod activity;
mod config;
mod host;
mod wake;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::Timelike;
use clap::{Parser, Subcommand};

use activity::{Awake, Monitor};
use config::{Config, Window};

#[derive(Parser)]
#[command(
    version,
    about = "Wake machines, and put them to sleep once their Herdr agents are done"
)]
struct Cli {
    /// Config file [default: $XDG_CONFIG_HOME/herdr-sleep/config.toml]
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Wake a configured machine and wait until it answers on SSH
    Wake {
        machine: String,
        /// Seconds to wait for SSH
        #[arg(long, default_value_t = 120)]
        timeout: u64,
        /// Send the wake signal without waiting
        #[arg(long)]
        no_wait: bool,
        /// Only report whether the machine is up (exit 0) or not (exit 1)
        #[arg(long)]
        check: bool,
        /// Print what would be sent instead of sending it
        #[arg(long)]
        dry_run: bool,
    },
    /// Sleep this machine once its agents have been done for a while
    WhenDone {
        /// How long agents must have been done [default: grace from [sleep], or 5m]
        #[arg(long)]
        grace: Option<String>,
        /// Keep waiting in the background
        #[arg(long)]
        detach: bool,
        /// Print the sleep command instead of running it
        #[arg(long)]
        dry_run: bool,
    },
    /// Cancel a pending when-done
    Cancel,
    /// Apply this machine's [sleep] policy
    Watch {
        /// Run as a Herdr hook: exit at once if there is no policy or another
        /// watcher is running, and exit when Herdr does
        #[arg(long)]
        hook: bool,
        /// Print the sleep command instead of running it
        #[arg(long)]
        dry_run: bool,
    },
    /// Show this machine's policy, what keeps it awake, and which machines are up
    Status,
}

const WHEN_DONE_POLL: Duration = Duration::from_secs(15);
const WATCH_POLL: Duration = Duration::from_secs(30);
/// Failed `herdr agent list` calls in a row before a hook gives up.
const HERDR_GONE: u32 = 3;
const DEFAULT_SLEEP: &str = "systemctl suspend";

fn state_dir() -> PathBuf {
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| config::home().join(".local/state"))
        .join("herdr-sleep")
}

fn pending_file() -> PathBuf {
    state_dir().join("when-done")
}

/// Where the watcher lock lives: the same file for the Herdr hook and a
/// service, so the two never run at once.
fn lock_file() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| PathBuf::from(d).join("herdr-sleep"))
        .unwrap_or_else(state_dir)
        .join("watch.lock")
}

/// Takes the watcher lock, or returns `None` if another process holds it.
/// The kernel releases it when this process exits, however it exits.
fn lock(wait: bool) -> Result<Option<File>> {
    let path = lock_file();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = File::create(&path).with_context(|| format!("opening {}", path.display()))?;
    if wait {
        f.lock().context("locking the watcher")?;
        return Ok(Some(f));
    }
    match f.try_lock() {
        Ok(()) => Ok(Some(f)),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e).context("locking the watcher"),
    }
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The pid and grace of a running when-done, from its record.
fn pending() -> Option<(u32, String)> {
    let raw = std::fs::read_to_string(pending_file()).ok()?;
    let (pid, grace) = raw.trim().split_once(' ')?;
    let pid = pid.parse().ok()?;
    alive(pid).then(|| (pid, grace.to_owned()))
}

fn sleep_now(command: &str, dry_run: bool) -> Result<()> {
    if dry_run {
        println!("would run: {command}");
        return Ok(());
    }
    println!("sleeping: {command}");
    let status = Command::new("sh").args(["-c", command]).status()?;
    if !status.success() {
        bail!("`{command}` failed ({status})");
    }
    Ok(())
}

/// Tracks how long the machine has been idle, across polls. A suspend in
/// between restarts the count, so a machine that just woke up gets a full
/// grace period.
struct Quiet {
    since: Option<Instant>,
    last: (Instant, SystemTime),
}

impl Quiet {
    fn new() -> Self {
        Quiet {
            since: None,
            last: (Instant::now(), SystemTime::now()),
        }
    }

    /// Records a poll and returns how long the machine has been idle.
    /// The monotonic clock stops during suspend; the wall clock does not.
    fn observe(&mut self, busy: bool) -> Duration {
        let (mono, wall) = (Instant::now(), SystemTime::now());
        let slept = wall
            .duration_since(self.last.1)
            .unwrap_or_default()
            .saturating_sub(mono - self.last.0)
            > Duration::from_secs(60);
        self.last = (mono, wall);
        if busy || slept {
            self.since = None;
            return Duration::ZERO;
        }
        mono - *self.since.get_or_insert(mono)
    }

    fn reset(&mut self) {
        self.since = None;
    }
}

fn busy_cpu(config: &Config) -> f64 {
    config
        .sleep
        .as_ref()
        .map_or(config::default_busy_cpu(), |s| s.busy_cpu)
}

fn summary(awake: &[Awake]) -> String {
    match awake {
        [] => "nothing keeps it awake".into(),
        [a] => a.to_string(),
        [a, rest @ ..] => format!("{a} (+{} more)", rest.len()),
    }
}

fn detach(grace: &str, dry_run: bool, config: &Option<PathBuf>) -> Result<()> {
    if let Some((pid, g)) = pending() {
        bail!("already waiting (pid {pid}, grace {g}); run `herdr-sleep cancel` first");
    }
    use std::os::unix::process::CommandExt;
    let mut args: Vec<String> = Vec::new();
    if let Some(c) = config {
        args.extend(["--config".into(), c.display().to_string()]);
    }
    args.extend(["when-done".into(), "--grace".into(), grace.into()]);
    if dry_run {
        args.push("--dry-run".into());
    }
    std::fs::create_dir_all(state_dir())?;
    let log = File::create(state_dir().join("when-done.log"))?;
    Command::new(std::env::current_exe()?)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .process_group(0)
        .spawn()
        .context("starting when-done in the background")?;
    println!(
        "{} sleeps once its agents have been done for {grace}. `herdr-sleep cancel` stops it.",
        host::hostname()
    );
    Ok(())
}

fn when_done(grace: &str, dry_run: bool, config: &Config) -> Result<()> {
    let grace_d = config::duration(grace)?;
    let command = config
        .sleep
        .as_ref()
        .map_or(DEFAULT_SLEEP, |s| s.command.as_str())
        .to_owned();
    if let Some((pid, g)) = pending().filter(|(pid, _)| *pid != std::process::id()) {
        bail!("already waiting (pid {pid}, grace {g}); run `herdr-sleep cancel` first");
    }
    std::fs::create_dir_all(state_dir())?;
    std::fs::write(pending_file(), format!("{} {grace}", std::process::id()))?;
    let _record = Record;
    println!(
        "{} sleeps once its agents have been done for {grace}",
        host::hostname()
    );
    let mut monitor = Monitor::primed(busy_cpu(config));
    let mut quiet = Quiet::new();
    let mut last = String::new();
    loop {
        let awake = monitor.poll(busy_cpu(config)).unwrap_or_default();
        let done_for = quiet.observe(!awake.is_empty());
        let now = summary(&awake);
        if now != last {
            println!("{now}");
            last = now;
        }
        if awake.is_empty() && done_for >= grace_d {
            return sleep_now(&command, dry_run);
        }
        std::thread::sleep(
            WHEN_DONE_POLL.min(grace_d.saturating_sub(done_for).max(Duration::from_secs(1))),
        );
    }
}

/// Removes the when-done record when its process ends normally.
struct Record;

impl Drop for Record {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(pending_file());
    }
}

fn cancel() -> Result<()> {
    let Some((pid, _)) = pending() else {
        let _ = std::fs::remove_file(pending_file());
        println!("nothing pending");
        return Ok(());
    };
    let status = Command::new("kill").arg(pid.to_string()).status()?;
    if !status.success() {
        bail!("could not stop pid {pid}");
    }
    let _ = std::fs::remove_file(pending_file());
    println!("cancelled; {} stays awake", host::hostname());
    Ok(())
}

struct Policy {
    command: String,
    grace: Duration,
    window: Option<Window>,
    require_locked: bool,
    busy_cpu: f64,
}

fn policy(config: &Config) -> Result<Option<Policy>> {
    let Some(s) = config.sleep.as_ref().filter(|s| s.auto) else {
        return Ok(None);
    };
    Ok(Some(Policy {
        command: s.command.clone(),
        grace: config::duration(&s.grace)?,
        window: s.window.as_deref().map(Window::parse).transpose()?,
        require_locked: s.require_locked,
        busy_cpu: s.busy_cpu,
    }))
}

fn minute_now() -> u32 {
    let t = chrono::Local::now();
    t.hour() * 60 + t.minute()
}

fn describe(p: &Policy) -> String {
    let mut s = format!("sleep after {} idle", config::human(p.grace));
    if let Some(w) = p.window {
        s += &format!(
            ", {:02}:{:02}-{:02}:{:02}",
            w.start / 60,
            w.start % 60,
            w.end / 60,
            w.end % 60
        );
    }
    if p.require_locked {
        s += ", only when locked";
    }
    s + &format!(", busy at {}% cpu", p.busy_cpu)
}

/// Polls every 30 seconds and sleeps once the machine is idle, inside the
/// window and locked. As a hook it exits when Herdr stops answering, so it
/// never outlives the server; the next startup or event starts a new one.
fn watch(hook: bool, dry_run: bool, config: &Config, path: &Path) -> Result<()> {
    let Some(p) = policy(config)? else {
        if hook {
            return Ok(());
        }
        bail!("no [sleep] with auto = true in {}", path.display());
    };
    let _lock = match lock(!hook)? {
        Some(l) => l,
        None => return Ok(()),
    };
    println!("{}: {}", host::hostname(), describe(&p));
    let mut monitor = Monitor::primed(p.busy_cpu);
    let mut quiet = Quiet::new();
    let mut misses = 0;
    loop {
        let awake = match monitor.poll(p.busy_cpu) {
            Some(a) => {
                misses = 0;
                a
            }
            None if hook => {
                misses += 1;
                if misses >= HERDR_GONE {
                    println!("herdr is gone; stopping");
                    return Ok(());
                }
                std::thread::sleep(WATCH_POLL);
                continue;
            }
            None => Vec::new(),
        };
        let done_for = quiet.observe(!awake.is_empty());
        let in_window = p.window.is_none_or(|w| w.contains(minute_now()));
        if in_window
            && awake.is_empty()
            && done_for >= p.grace
            && (!p.require_locked || host::locked())
        {
            if let Err(e) = sleep_now(&p.command, dry_run) {
                eprintln!("{e:#}");
            }
            quiet.reset();
        }
        std::thread::sleep(WATCH_POLL);
    }
}

fn status(config: &Config, path: &Path) -> Result<()> {
    let name = host::hostname();
    println!("{name}");
    let p = policy(config)?;
    match &p {
        Some(p) => {
            let in_window = p.window.is_none_or(|w| w.contains(minute_now()));
            println!("  policy   {}", describe(p));
            println!(
                "  now      {} · {}",
                if in_window {
                    "in window"
                } else {
                    "outside window"
                },
                if host::locked() { "locked" } else { "unlocked" },
            );
        }
        None => println!(
            "  policy   manual only (no auto = true in {})",
            path.display()
        ),
    }
    match pending() {
        Some((pid, grace)) => {
            println!("  pending  sleeps when agents are done (grace {grace}, pid {pid})")
        }
        None => println!("  pending  nothing"),
    }
    match lock(false)? {
        Some(_) => println!("  watcher  not running"),
        None => println!("  watcher  running"),
    }

    let up = std::thread::scope(|s| {
        let probes = s.spawn(|| {
            config
                .machines
                .iter()
                .map(|(n, m)| wake::reachable(wake::host(n, m), Duration::from_secs(3)))
                .collect::<Vec<bool>>()
        });
        let busy = busy_cpu(config);
        let awake = activity::agents().and_then(|_| Monitor::primed(busy).poll(busy));
        match awake {
            None => println!("  awake    herdr is not running"),
            Some(a) if a.is_empty() => {
                println!("  awake    nothing: no agent working, no busy background command")
            }
            Some(a) => {
                for (i, x) in a.iter().enumerate() {
                    println!("  {}{x}", if i == 0 { "awake    " } else { "         " });
                }
            }
        }
        probes.join().unwrap_or_default()
    });
    if config.machines.is_empty() {
        return Ok(());
    }
    println!();
    for ((n, m), up) in config.machines.iter().zip(up) {
        let how = wake::method(m).unwrap_or_else(|| "no wake method".into());
        println!("  {n:<12} {:<6} {how}", if up { "up" } else { "down" });
    }
    Ok(())
}

fn run_wake(
    name: &str,
    timeout: u64,
    no_wait: bool,
    check: bool,
    dry_run: bool,
    config: &Config,
) -> Result<bool> {
    let default = config::Machine::default();
    let m = config.machines.get(name);
    let host = wake::host(name, m.unwrap_or(&default));
    if wake::reachable(host, Duration::from_secs(3)) {
        println!("{name} is up");
        return Ok(true);
    }
    if check {
        println!("{name} is down");
        return Ok(false);
    }
    let Some(m) = m else {
        bail!("{name} is down and has no [machines.{name}] entry");
    };
    let how = wake::method(m).with_context(|| {
        format!("no way to wake {name}: set mac or wake under [machines.{name}]")
    })?;
    if dry_run {
        println!("would wake {name}: {how}, then wait {timeout}s for {host}:22");
        return Ok(true);
    }
    println!("waking {name}: {how}");
    wake::send(name, m)?;
    if no_wait {
        return Ok(true);
    }
    if wake::wait(host, Duration::from_secs(timeout)) {
        println!("{name} is up");
        Ok(true)
    } else {
        eprintln!("{name} did not answer on SSH within {timeout}s");
        Ok(false)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli.config.clone().unwrap_or_else(config::path);
    let config = config::load(&path)?;
    match cli.cmd {
        Cmd::Wake {
            machine,
            timeout,
            no_wait,
            check,
            dry_run,
        } => {
            if !run_wake(&machine, timeout, no_wait, check, dry_run, &config)? {
                std::process::exit(1);
            }
            Ok(())
        }
        Cmd::WhenDone {
            grace,
            detach: wait_elsewhere,
            dry_run,
        } => {
            let grace = grace.unwrap_or_else(|| {
                config
                    .sleep
                    .as_ref()
                    .map_or_else(config::default_grace, |s| s.grace.clone())
            });
            config::duration(&grace)?;
            if wait_elsewhere {
                detach(&grace, dry_run, &cli.config)
            } else {
                when_done(&grace, dry_run, &config)
            }
        }
        Cmd::Cancel => cancel(),
        Cmd::Watch { hook, dry_run } => watch(hook, dry_run, &config, &path),
        Cmd::Status => status(&config, &path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_counts_from_first_idle_poll() {
        let mut q = Quiet::new();
        assert_eq!(q.observe(true), Duration::ZERO);
        assert_eq!(q.observe(false), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        assert!(q.observe(false) >= Duration::from_millis(20));
        assert_eq!(q.observe(true), Duration::ZERO);
    }
}
