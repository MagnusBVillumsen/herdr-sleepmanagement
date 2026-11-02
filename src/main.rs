mod config;
mod host;
mod wake;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::Timelike;
use clap::{Parser, Subcommand};

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
    /// Sleep this machine once no Herdr agent has been working for a while
    WhenDone {
        /// How long agents must have stopped working
        #[arg(long, default_value = "5m")]
        grace: String,
        /// Keep waiting in the background
        #[arg(long)]
        detach: bool,
        /// Print the sleep command instead of running it
        #[arg(long)]
        dry_run: bool,
    },
    /// Cancel a pending when-done
    Cancel,
    /// Apply this machine's [sleep] policy until stopped (for a user service)
    Watch {
        /// Print the sleep command instead of running it
        #[arg(long)]
        dry_run: bool,
    },
    /// Show this machine's policy and which machines are up
    Status,
}

const WHEN_DONE_POLL: Duration = Duration::from_secs(15);
const WATCH_POLL: Duration = Duration::from_secs(30);
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

/// Tracks how long agents have been done, across polls. A suspend in between
/// restarts the count, so a machine that just woke up gets a full grace period.
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

    /// Records a poll and returns how long agents have been done.
    fn observe(&mut self, working: usize) -> Duration {
        let (mono, wall) = (Instant::now(), SystemTime::now());
        let slept = wall
            .duration_since(self.last.1)
            .unwrap_or_default()
            .saturating_sub(mono - self.last.0)
            > Duration::from_secs(60);
        self.last = (mono, wall);
        if working > 0 || slept {
            self.since = None;
            return Duration::ZERO;
        }
        mono - *self.since.get_or_insert(mono)
    }

    fn reset(&mut self) {
        self.since = None;
    }
}

/// The user manager has its own environment, so pass on what finds Herdr.
fn forwarded_env() -> Vec<String> {
    std::env::vars()
        .filter(|(k, _)| k == "PATH" || k.starts_with("HERDR_") || k.starts_with("XDG_"))
        .map(|(k, v)| format!("--setenv={k}={v}"))
        .collect()
}

fn detach(grace: &str, dry_run: bool, config: &Option<PathBuf>) -> Result<()> {
    if let Some((pid, g)) = pending() {
        bail!("already waiting (pid {pid}, grace {g}); run `herdr-sleep cancel` first");
    }
    let exe = std::env::current_exe()?;
    let mut args: Vec<String> = Vec::new();
    if let Some(c) = config {
        args.extend(["--config".into(), c.display().to_string()]);
    }
    args.extend(["when-done".into(), "--grace".into(), grace.into()]);
    if dry_run {
        args.push("--dry-run".into());
    }
    // A user unit outlives the terminal or Herdr popup that started it.
    let unit = Command::new("systemd-run")
        .args([
            "--user",
            "--collect",
            "--quiet",
            "--unit=herdr-sleep-when-done",
        ])
        .args(forwarded_env())
        .arg(&exe)
        .args(&args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !unit.is_ok_and(|s| s.success()) {
        use std::os::unix::process::CommandExt;
        std::fs::create_dir_all(state_dir())?;
        let log = std::fs::File::create(state_dir().join("when-done.log"))?;
        Command::new(&exe)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .process_group(0)
            .spawn()
            .context("starting when-done in the background")?;
    }
    println!(
        "{} sleeps once no agent has been working for {grace}. `herdr-sleep cancel` stops it.",
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
    let mut quiet = Quiet::new();
    let mut last_working = usize::MAX;
    loop {
        let working = host::working_agents();
        let done_for = quiet.observe(working);
        if working != last_working {
            match working {
                0 => println!("no agent working; sleeping in {}", config::human(grace_d)),
                n => println!("waiting for {n} working agent(s)"),
            }
            last_working = working;
        }
        if working == 0 && done_for >= grace_d {
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
}

fn policy(config: &Config) -> Result<Option<Policy>> {
    let Some(s) = &config.sleep else {
        return Ok(None);
    };
    Ok(Some(Policy {
        command: s.command.clone(),
        grace: config::duration(&s.grace)?,
        window: s.window.as_deref().map(Window::parse).transpose()?,
        require_locked: s.require_locked,
    }))
}

fn minute_now() -> u32 {
    let t = chrono::Local::now();
    t.hour() * 60 + t.minute()
}

fn describe(p: &Policy) -> String {
    let mut s = format!(
        "sleep after {} without working agents",
        config::human(p.grace)
    );
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
    s
}

fn watch(dry_run: bool, config: &Config, path: &Path) -> Result<()> {
    let Some(p) = policy(config)? else {
        bail!("no [sleep] section in {}", path.display());
    };
    println!("{}: {}", host::hostname(), describe(&p));
    let mut quiet = Quiet::new();
    loop {
        let working = host::working_agents();
        let done_for = quiet.observe(working);
        let in_window = p.window.is_none_or(|w| w.contains(minute_now()));
        if in_window && working == 0 && done_for >= p.grace && (!p.require_locked || host::locked())
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
    match policy(config)? {
        Some(p) => {
            println!("  policy   {}", describe(&p));
            let in_window = p.window.is_none_or(|w| w.contains(minute_now()));
            println!(
                "  now      {} · {} · {} working agent(s)",
                if in_window {
                    "in window"
                } else {
                    "outside window"
                },
                if host::locked() { "locked" } else { "unlocked" },
                host::working_agents()
            );
        }
        None => {
            println!("  policy   none (no [sleep] in {})", path.display());
            println!("  now      {} working agent(s)", host::working_agents());
        }
    }
    match pending() {
        Some((pid, grace)) => {
            println!("  pending  sleeps when agents are done (grace {grace}, pid {pid})")
        }
        None => println!("  pending  nothing"),
    }
    if config.machines.is_empty() {
        return Ok(());
    }
    println!();
    let up: Vec<bool> = std::thread::scope(|s| {
        let handles: Vec<_> = config
            .machines
            .iter()
            .map(|(n, m)| {
                s.spawn(move || wake::reachable(wake::host(n, m), Duration::from_secs(3)))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or(false))
            .collect()
    });
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
            detach: true,
            dry_run,
        } => {
            config::duration(&grace)?;
            detach(&grace, dry_run, &cli.config)
        }
        Cmd::WhenDone { grace, dry_run, .. } => when_done(&grace, dry_run, &config),
        Cmd::Cancel => cancel(),
        Cmd::Watch { dry_run } => watch(dry_run, &config, &path),
        Cmd::Status => status(&config, &path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quiet_counts_from_first_idle_poll() {
        let mut q = Quiet::new();
        assert_eq!(q.observe(2), Duration::ZERO);
        assert_eq!(q.observe(0), Duration::ZERO);
        std::thread::sleep(Duration::from_millis(20));
        assert!(q.observe(0) >= Duration::from_millis(20));
        assert_eq!(q.observe(1), Duration::ZERO);
    }
}
