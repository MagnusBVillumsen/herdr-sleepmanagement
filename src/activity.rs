//! What keeps this machine awake: Herdr agents that are working, and the
//! background commands they started.

use std::collections::HashMap;
use std::process::Command;
use std::time::{Duration, Instant};

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub ppid: u32,
    /// CPU seconds used so far, including reaped children where the OS
    /// reports them.
    pub cpu: f64,
    /// Seconds since the process started.
    pub age: f64,
    pub args: String,
}

pub type Table = HashMap<u32, Proc>;

pub fn snapshot() -> Table {
    if cfg!(target_os = "linux") && std::path::Path::new("/proc/self/stat").exists() {
        return proc_table();
    }
    Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,etime=,time=,args="])
        .output()
        .map(|o| parse_ps(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Linux reports CPU time in USER_HZ, which is 100 on every architecture.
fn proc_table() -> Table {
    let mut t = Table::new();
    let uptime = std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
        .unwrap_or(0.0);
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return t;
    };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
            continue;
        };
        let Some(p) = parse_stat(&stat, uptime) else {
            continue;
        };
        let args = std::fs::read(e.path().join("cmdline"))
            .map(|b| {
                b.split(|&c| c == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default();
        t.insert(pid, Proc { args, ..p });
    }
    t
}

fn parse_stat(stat: &str, uptime: f64) -> Option<Proc> {
    let f: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let ticks: f64 = (11..15)
        .map(|i| f.get(i).and_then(|v| v.parse::<f64>().ok()))
        .sum::<Option<f64>>()?;
    let started: f64 = f.get(19)?.parse().ok()?;
    Some(Proc {
        ppid: f.get(1)?.parse().ok()?,
        cpu: ticks / 100.0,
        age: (uptime - started / 100.0).max(0.0),
        args: String::new(),
    })
}

pub fn parse_ps(out: &str) -> Table {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let age = cpu_time(it.next()?)?;
            let cpu = cpu_time(it.next()?)?;
            let args = it.collect::<Vec<_>>().join(" ");
            Some((
                pid,
                Proc {
                    ppid,
                    cpu,
                    age,
                    args,
                },
            ))
        })
        .collect()
}

/// `[[dd-]hh:]mm:ss[.cc]`, as printed by `ps -o time` and `-o etime`.
fn cpu_time(s: &str) -> Option<f64> {
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<f64>().ok()?, r),
        None => (0.0, s),
    };
    let secs = rest
        .split(':')
        .try_fold(0.0, |acc, p| p.parse::<f64>().ok().map(|v| acc * 60.0 + v))?;
    Some(days * 86400.0 + secs)
}

/// A shell running a command string, which is how agents run their tools:
/// `zsh -c ...` (Claude Code), `bash -lc ...` (Codex), `bash -c ...` (Pi).
/// Language servers and MCP servers are not started this way.
fn tool_shell(args: &str) -> bool {
    let mut it = args.split_whitespace();
    let Some(exe) = it.next() else { return false };
    let name = exe
        .rsplit('/')
        .next()
        .unwrap_or(exe)
        .trim_start_matches('-');
    let flag = it.next().unwrap_or_default();
    matches!(
        name,
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "mksh" | "fish"
    ) && flag.len() > 1
        && flag.starts_with('-')
        && !flag.starts_with("--")
        && flag[1..].chars().all(|c| c.is_ascii_alphabetic())
        && flag.contains('c')
}

fn children(t: &Table) -> HashMap<u32, Vec<u32>> {
    let mut c: HashMap<u32, Vec<u32>> = HashMap::new();
    for (&pid, p) in t {
        c.entry(p.ppid).or_default().push(pid);
    }
    c
}

fn subtree(kids: &HashMap<u32, Vec<u32>>, pid: u32) -> Vec<u32> {
    let mut out = vec![pid];
    let mut i = 0;
    while i < out.len() {
        if let Some(k) = kids.get(&out[i]) {
            out.extend(k);
        }
        i += 1;
    }
    out
}

/// The outermost tool shells below `root`.
fn tool_shells(t: &Table, kids: &HashMap<u32, Vec<u32>>, root: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut todo = kids.get(&root).cloned().unwrap_or_default();
    while let Some(pid) = todo.pop() {
        if t.get(&pid).is_some_and(|p| tool_shell(&p.args)) {
            out.push(pid);
        } else if let Some(k) = kids.get(&pid) {
            todo.extend(k);
        }
    }
    out.sort();
    out
}

/// CPU used by `pid` since `prev`; a process not in `prev` is new.
fn used(prev: &Table, now: &Table, pid: u32) -> f64 {
    let n = &now[&pid];
    match prev.get(&pid) {
        Some(p) if p.ppid == n.ppid && p.cpu <= n.cpu => n.cpu - p.cpu,
        _ => n.cpu,
    }
}

fn short(args: &str) -> String {
    let mut it = args.split_whitespace();
    let exe = it.next().unwrap_or_default();
    let mut s = std::iter::once(match exe.strip_prefix('/') {
        Some(_) => exe.rsplit('/').next().unwrap_or(exe),
        None => exe,
    })
    .chain(it)
    .collect::<Vec<_>>()
    .join(" ");
    if s.chars().count() > 50 {
        s = s.chars().take(49).collect::<String>() + "…";
    }
    s
}

/// When a background command keeps the machine awake.
#[derive(Debug, Clone, Copy)]
pub struct Busy {
    /// Percent of one core a background command must use.
    pub cpu: f64,
    /// A background command younger than this counts whatever it uses: it is
    /// usually waiting on something (a container build, a download, CI), and
    /// the agent picks up again when it ends. Older ones, like a dev server
    /// left running, only count while they use CPU.
    pub young: Duration,
}

#[derive(Debug, PartialEq)]
pub struct Background {
    pub cmd: String,
    pub age: f64,
    /// Share of one core since the last poll, when it is what counts.
    pub cpu: Option<f64>,
}

/// Background commands under `root` that keep the machine awake, one per
/// tool shell: young ones by their first command, busy ones by their
/// busiest. CPU is measured over the `secs` since `prev`.
pub fn background(
    prev: Option<&Table>,
    now: &Table,
    secs: f64,
    root: u32,
    busy: Busy,
) -> Vec<Background> {
    let kids = children(now);
    tool_shells(now, &kids, root)
        .into_iter()
        .filter_map(|sh| {
            let tree = subtree(&kids, sh);
            let below = |p: &&u32| **p != sh || tree.len() == 1;
            let age = now[&sh].age;
            if age < busy.young.as_secs_f64() {
                let first = tree.iter().filter(below).min()?;
                return Some(Background {
                    cmd: short(&now[first].args),
                    age,
                    cpu: None,
                });
            }
            let prev = prev?;
            let total: f64 = tree.iter().map(|&p| used(prev, now, p)).sum();
            let pct = total / secs * 100.0;
            if pct < busy.cpu {
                return None;
            }
            let top = tree
                .iter()
                .filter(below)
                .max_by(|&&a, &&b| used(prev, now, a).total_cmp(&used(prev, now, b)))?;
            Some(Background {
                cmd: short(&now[top].args),
                age,
                cpu: Some(pct),
            })
        })
        .collect()
}

fn herdr(args: &[&str]) -> Option<Value> {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let out = Command::new(bin).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .map(|mut v| v["result"].take())
}

pub struct Agent {
    pub pane: String,
    pub agent: String,
    pub working: bool,
}

/// Herdr's agents, or `None` if Herdr is not running.
pub fn agents() -> Option<Vec<Agent>> {
    let v = herdr(&["agent", "list"])?;
    Some(
        v["agents"]
            .as_array()?
            .iter()
            .map(|a| Agent {
                pane: a["pane_id"].as_str().unwrap_or_default().into(),
                agent: a["agent"].as_str().unwrap_or("agent").into(),
                working: a["agent_status"] == "working",
            })
            .collect(),
    )
}

fn shell_pid(pane: &str) -> Option<u32> {
    let v = herdr(&["pane", "process-info", "--pane", pane])?;
    v["process_info"]["shell_pid"].as_u64().map(|p| p as u32)
}

pub enum Reason {
    Working,
    Background(Background),
}

pub struct Awake {
    pub pane: String,
    pub agent: String,
    pub reason: Reason,
}

impl std::fmt::Display for Awake {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{} {}  ", self.pane, self.agent)?;
        match &self.reason {
            Reason::Working => write!(f, "working"),
            Reason::Background(b) => {
                let age = crate::config::human(Duration::from_secs(b.age as u64));
                match b.cpu {
                    Some(pct) => write!(f, "background: {} {pct:.0}% cpu", b.cmd),
                    None => write!(f, "background: {} (running {age})", b.cmd),
                }
            }
        }
    }
}

/// Polls agents and compares process tables between polls.
pub struct Monitor {
    busy: Busy,
    prev: Option<(Instant, Table)>,
}

impl Monitor {
    /// Takes a first measurement, so the next poll can see CPU use.
    pub fn primed(busy: Busy) -> Self {
        let mut m = Monitor { busy, prev: None };
        m.poll();
        std::thread::sleep(std::time::Duration::from_secs(2));
        m
    }

    /// What keeps the machine awake now, or `None` if Herdr is not running.
    /// CPU use shows from the second poll on. This process and its
    /// own children are left out, in case it was started from an agent.
    pub fn poll(&mut self) -> Option<Vec<Awake>> {
        let agents = agents()?;
        let (at, mut now) = (Instant::now(), snapshot());
        let me = std::process::id();
        now.retain(|&pid, p| pid != me && p.ppid != me);
        let mut out = Vec::new();
        for a in agents {
            if a.working {
                out.push(Awake {
                    pane: a.pane,
                    agent: a.agent,
                    reason: Reason::Working,
                });
                continue;
            }
            let Some(root) = shell_pid(&a.pane) else {
                continue;
            };
            let prev = self.prev.as_ref();
            let secs = prev.map_or(1.0, |(then, _)| (at - *then).as_secs_f64().max(0.001));
            for b in background(prev.map(|(_, t)| t), &now, secs, root, self.busy) {
                out.push(Awake {
                    pane: a.pane.clone(),
                    agent: a.agent.clone(),
                    reason: Reason::Background(b),
                });
            }
        }
        self.prev = Some((at, now));
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const OLD: f64 = 46.0 * 3600.0;

    fn table(rows: &[(u32, u32, f64, &str)]) -> Table {
        rows.iter()
            .map(|&(pid, ppid, cpu, args)| {
                (
                    pid,
                    Proc {
                        ppid,
                        cpu,
                        age: OLD,
                        args: args.into(),
                    },
                )
            })
            .collect()
    }

    const BUSY: Busy = Busy {
        cpu: 10.0,
        young: Duration::from_secs(3 * 3600),
    };

    fn cmds(b: Vec<Background>) -> Vec<String> {
        b.into_iter().map(|b| b.cmd).collect()
    }

    const SNAP: &str = "/run/current-system/sw/bin/zsh -c source /home/u/.claude/shell-snapshots/s.sh && eval 'cargo build'";

    fn base(server: f64, cargo: f64, rustc: Option<f64>) -> Table {
        let mut rows = vec![
            (10, 1, 0.0, "/run/current-system/sw/bin/zsh"),
            (11, 10, 3000.0, "claude"),
            (
                12,
                11,
                6.0 + cargo,
                "/nix/store/x-nodejs/bin/node typescript-language-server --stdio",
            ),
            (
                13,
                11,
                0.0,
                "/nix/store/x-bash/bin/bash /etc/profiles/per-user/u/bin/clangd --background-index",
            ),
            (
                14,
                13,
                2.0 + cargo,
                "/nix/store/x/bin/clangd-unwrapped --background-index",
            ),
            (
                20,
                11,
                0.0,
                "/run/current-system/sw/bin/zsh -c source /home/u/.claude/shell-snapshots/s.sh && ./build/server &",
            ),
            (21, 20, 1142.0 + server, "./build/server"),
            (30, 11, 0.0, SNAP),
            (31, 30, 10.0 + cargo, "/nix/store/x-cargo/bin/cargo build"),
            (99, 1, 500.0, "unrelated"),
        ];
        if let Some(r) = rustc {
            rows.push((32, 31, r, "/nix/store/x-rustc/bin/rustc --crate-name foo"));
        }
        table(&rows)
    }

    #[test]
    fn idle_server_does_not_count_but_a_build_does() {
        let prev = base(0.0, 0.0, None);
        let now = base(0.3, 3.0, Some(25.0));
        let busy = background(Some(&prev), &now, 30.0, 10, BUSY);
        assert_eq!(busy.len(), 1);
        assert_eq!(busy[0].cmd, "rustc --crate-name foo");
        let pct = busy[0].cpu.unwrap();
        assert!((pct - 93.3).abs() < 0.1, "{pct}");

        let quiet = base(0.3, 0.0, None);
        assert!(background(Some(&prev), &quiet, 30.0, 10, BUSY).is_empty());
        assert_eq!(
            cmds(background(
                Some(&prev),
                &base(6.0, 0.0, None),
                30.0,
                10,
                BUSY
            )),
            ["./build/server"]
        );
    }

    #[test]
    fn young_commands_count_without_cpu() {
        let mut now = base(0.0, 0.0, None);
        for (pid, age) in [(30, 480.0), (31, 480.0)] {
            now.get_mut(&pid).unwrap().age = age;
        }
        let young = background(None, &now, 1.0, 10, BUSY);
        assert_eq!(cmds(young), ["cargo build"]);
        let prev = now.clone();
        assert_eq!(
            background(Some(&prev), &now, 30.0, 10, BUSY)[0],
            Background {
                cmd: "cargo build".into(),
                age: 480.0,
                cpu: None
            }
        );
    }

    #[test]
    fn language_servers_and_other_panes_are_ignored() {
        let prev = base(0.0, 0.0, None);
        let mut now = base(0.0, 0.0, None);
        now.get_mut(&12).unwrap().cpu += 30.0;
        now.get_mut(&14).unwrap().cpu += 30.0;
        now.get_mut(&99).unwrap().cpu += 30.0;
        assert!(background(Some(&prev), &now, 30.0, 10, BUSY).is_empty());
    }

    #[test]
    fn recognises_tool_shells() {
        assert!(tool_shell(SNAP));
        assert!(tool_shell("bash -lc 'npm test'"));
        assert!(tool_shell("/bin/sh -c make"));
        assert!(!tool_shell("/run/current-system/sw/bin/zsh"));
        assert!(!tool_shell("bash /etc/profiles/per-user/u/bin/clangd"));
        assert!(!tool_shell("bash --norc"));
        assert!(!tool_shell("node -c x"));
    }

    #[test]
    fn parses_process_listings() {
        let t = parse_ps(
            "  1  0 3-00:00:00 00:00:48 /sbin/init\n 7 1 05:00 1-02:03:04 cargo build\n 8 7 00:10 0:01.50 rustc\n",
        );
        assert_eq!(t[&7].age, 300.0);
        assert_eq!(t[&7].cpu, 86400.0 + 7384.0);
        assert_eq!(t[&8].cpu, 1.5);
        assert_eq!(t[&7].args, "cargo build");
        let p = parse_stat(
            "42 (a (b) c) S 7 1 1 0 -1 0 0 0 0 0 150 50 30 20 20 0 1 0 90000 0",
            1000.0,
        )
        .unwrap();
        assert_eq!((p.ppid, p.cpu, p.age), (7, 2.5, 100.0));
    }
}
