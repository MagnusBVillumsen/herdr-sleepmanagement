//! What keeps this machine awake: Herdr agents that are working, and the
//! background commands they started.

use std::collections::HashMap;
use std::process::Command;
use std::time::Instant;

use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Proc {
    pub ppid: u32,
    /// CPU seconds used so far, including reaped children where the OS
    /// reports them.
    pub cpu: f64,
    pub args: String,
}

pub type Table = HashMap<u32, Proc>;

pub fn snapshot() -> Table {
    if cfg!(target_os = "linux") && std::path::Path::new("/proc/self/stat").exists() {
        return proc_table();
    }
    Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,time=,args="])
        .output()
        .map(|o| parse_ps(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_default()
}

/// Linux reports CPU time in USER_HZ, which is 100 on every architecture.
fn proc_table() -> Table {
    let mut t = Table::new();
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
        let Some(p) = parse_stat(&stat) else { continue };
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

fn parse_stat(stat: &str) -> Option<Proc> {
    let f: Vec<&str> = stat.rsplit_once(')')?.1.split_whitespace().collect();
    let ticks: f64 = (11..15)
        .map(|i| f.get(i).and_then(|v| v.parse::<f64>().ok()))
        .sum::<Option<f64>>()?;
    Some(Proc {
        ppid: f.get(1)?.parse().ok()?,
        cpu: ticks / 100.0,
        args: String::new(),
    })
}

pub fn parse_ps(out: &str) -> Table {
    out.lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let cpu = cpu_time(it.next()?)?;
            let args = it.collect::<Vec<_>>().join(" ");
            Some((pid, Proc { ppid, cpu, args }))
        })
        .collect()
}

/// `[[dd-]hh:]mm:ss[.cc]`, as printed by `ps -o time`.
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

/// Background commands under `root` that used at least `busy_cpu` percent
/// of one core over the `secs` between the two tables: the busiest command
/// in each tool shell's tree, and the tree's total share.
pub fn background(
    prev: &Table,
    now: &Table,
    secs: f64,
    root: u32,
    busy_cpu: f64,
) -> Vec<(String, f64)> {
    let kids = children(now);
    tool_shells(now, &kids, root)
        .into_iter()
        .filter_map(|sh| {
            let tree = subtree(&kids, sh);
            let total: f64 = tree.iter().map(|&p| used(prev, now, p)).sum();
            let pct = total / secs * 100.0;
            if pct < busy_cpu {
                return None;
            }
            let top = tree
                .iter()
                .filter(|&&p| p != sh || tree.len() == 1)
                .max_by(|&&a, &&b| used(prev, now, a).total_cmp(&used(prev, now, b)))?;
            Some((short(&now[top].args), pct))
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
    Background(String, f64),
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
            Reason::Background(cmd, pct) => write!(f, "background: {cmd} {pct:.0}% cpu"),
        }
    }
}

/// Polls agents and compares process tables between polls.
#[derive(Default)]
pub struct Monitor {
    prev: Option<(Instant, Table)>,
}

impl Monitor {
    /// Takes a first measurement, so the next poll can see background work.
    pub fn primed(busy_cpu: f64) -> Self {
        let mut m = Monitor::default();
        m.poll(busy_cpu);
        std::thread::sleep(std::time::Duration::from_secs(2));
        m
    }

    /// What keeps the machine awake now, or `None` if Herdr is not running.
    /// Background work shows from the second poll on. This process and its
    /// own children are left out, in case it was started from an agent.
    pub fn poll(&mut self, busy_cpu: f64) -> Option<Vec<Awake>> {
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
            let (Some((then, prev)), Some(root)) = (&self.prev, shell_pid(&a.pane)) else {
                continue;
            };
            let secs = (at - *then).as_secs_f64().max(0.001);
            for (cmd, pct) in background(prev, &now, secs, root, busy_cpu) {
                out.push(Awake {
                    pane: a.pane.clone(),
                    agent: a.agent.clone(),
                    reason: Reason::Background(cmd, pct),
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

    fn table(rows: &[(u32, u32, f64, &str)]) -> Table {
        rows.iter()
            .map(|&(pid, ppid, cpu, args)| {
                (
                    pid,
                    Proc {
                        ppid,
                        cpu,
                        args: args.into(),
                    },
                )
            })
            .collect()
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
        let busy = background(&prev, &now, 30.0, 10, 10.0);
        assert_eq!(busy.len(), 1);
        let (cmd, pct) = &busy[0];
        assert_eq!(cmd, "rustc --crate-name foo");
        assert!((pct - 93.3).abs() < 0.1, "{pct}");

        let quiet = base(0.3, 0.0, None);
        assert!(background(&prev, &quiet, 30.0, 10, 10.0).is_empty());
        assert_eq!(
            background(&prev, &base(6.0, 0.0, None), 30.0, 10, 10.0)[0].0,
            "./build/server"
        );
    }

    #[test]
    fn language_servers_and_other_panes_are_ignored() {
        let prev = base(0.0, 0.0, None);
        let mut now = base(0.0, 0.0, None);
        now.get_mut(&12).unwrap().cpu += 30.0;
        now.get_mut(&14).unwrap().cpu += 30.0;
        now.get_mut(&99).unwrap().cpu += 30.0;
        assert!(background(&prev, &now, 30.0, 10, 10.0).is_empty());
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
            "  1  0 00:00:48 /sbin/init\n 7 1 1-02:03:04 cargo build\n 8 7 0:01.50 rustc\n",
        );
        assert_eq!(t[&7].cpu, 86400.0 + 7384.0);
        assert_eq!(t[&8].cpu, 1.5);
        assert_eq!(t[&7].args, "cargo build");
        let p = parse_stat("42 (a (b) c) S 7 1 1 0 -1 0 0 0 0 0 150 50 30 20 20 0").unwrap();
        assert_eq!((p.ppid, p.cpu), (7, 2.5));
    }
}
