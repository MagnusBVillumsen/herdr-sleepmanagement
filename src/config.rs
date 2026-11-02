use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub machines: BTreeMap<String, Machine>,
    pub sleep: Option<Sleep>,
}

/// How to wake a machine.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    /// MAC address for a Wake-on-LAN magic packet.
    pub mac: Option<String>,
    /// An always-on SSH host on the machine's LAN that sends the packet.
    pub relay: Option<String>,
    /// The host whose SSH port shows the machine is up; default its name.
    pub host: Option<String>,
    /// A shell command that wakes the machine, instead of `mac`.
    pub wake: Option<String>,
}

/// How this machine sleeps. `when-done` uses it as given; `watch` only acts
/// when `auto` is on.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sleep {
    #[serde(default = "default_command")]
    pub command: String,
    #[serde(default = "default_grace")]
    pub grace: String,
    /// Sleep on its own, without anyone asking, under the rules below.
    #[serde(default)]
    pub auto: bool,
    pub window: Option<String>,
    #[serde(default = "default_true")]
    pub require_locked: bool,
    /// Percent of one core a background command must use to count as busy.
    #[serde(default = "default_busy_cpu")]
    pub busy_cpu: f64,
    /// Background commands younger than this count whatever CPU they use.
    #[serde(default = "default_background_max")]
    pub background_max: String,
}

fn default_command() -> String {
    "systemctl suspend".into()
}

pub fn default_grace() -> String {
    "5m".into()
}

pub fn default_background_max() -> String {
    "3h".into()
}

pub fn default_busy_cpu() -> f64 {
    10.0
}

fn default_true() -> bool {
    true
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"));
    base.join("herdr-sleep/config.toml")
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

pub fn load(path: &std::path::Path) -> Result<Config> {
    match std::fs::read_to_string(path) {
        Ok(raw) => toml::from_str(&raw).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Parses durations such as `90s`, `10m`, `1h30m` or a bare number of minutes.
pub fn duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if let Ok(m) = s.parse::<u64>() {
        return Ok(Duration::from_secs(m * 60));
    }
    let mut total = 0;
    let mut num = String::new();
    for c in s.chars() {
        match c {
            '0'..='9' => num.push(c),
            'h' | 'm' | 's' if !num.is_empty() => {
                let n: u64 = num.parse()?;
                total += n * match c {
                    'h' => 3600,
                    'm' => 60,
                    _ => 1,
                };
                num.clear();
            }
            _ => bail!("bad duration {s:?}: use e.g. 30s, 10m or 1h30m"),
        }
    }
    if !num.is_empty() || total == 0 {
        bail!("bad duration {s:?}: use e.g. 30s, 10m or 1h30m");
    }
    Ok(Duration::from_secs(total))
}

pub fn human(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 3600, s % 3600 / 60, s % 60) {
        (0, 0, s) => format!("{s}s"),
        (0, m, 0) => format!("{m}m"),
        (0, m, s) => format!("{m}m{s}s"),
        (h, 0, _) => format!("{h}h"),
        (h, m, _) => format!("{h}h{m}m"),
    }
}

/// A daily local-time window such as `23:00-07:00`, as minutes after midnight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    pub start: u32,
    pub end: u32,
}

impl Window {
    pub fn parse(s: &str) -> Result<Self> {
        let clock = |t: &str| -> Result<u32> {
            let (h, m) = t.trim().split_once(':').context("expected HH:MM")?;
            let (h, m): (u32, u32) = (h.parse()?, m.parse()?);
            if h > 23 || m > 59 {
                bail!("expected HH:MM");
            }
            Ok(h * 60 + m)
        };
        let (a, b) = s
            .split_once('-')
            .with_context(|| format!("bad window {s:?}: use e.g. 23:00-07:00"))?;
        let w = Window {
            start: clock(a).with_context(|| format!("bad window {s:?}"))?,
            end: clock(b).with_context(|| format!("bad window {s:?}"))?,
        };
        if w.start == w.end {
            bail!("bad window {s:?}: start and end are the same");
        }
        Ok(w)
    }

    /// Whether `minute` (after midnight) is inside; windows may wrap midnight.
    pub fn contains(self, minute: u32) -> bool {
        if self.start < self.end {
            (self.start..self.end).contains(&minute)
        } else {
            minute >= self.start || minute < self.end
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration("90s").unwrap().as_secs(), 90);
        assert_eq!(duration("10m").unwrap().as_secs(), 600);
        assert_eq!(duration("1h30m").unwrap().as_secs(), 5400);
        assert_eq!(duration("5").unwrap().as_secs(), 300);
        assert!(duration("10x").is_err());
        assert!(duration("m").is_err());
        assert!(duration("10m5").is_err());
        assert_eq!(human(Duration::from_secs(5400)), "1h30m");
    }

    #[test]
    fn windows() {
        let night = Window::parse("23:00-07:00").unwrap();
        assert!(night.contains(23 * 60) && night.contains(2 * 60) && !night.contains(7 * 60));
        assert!(!night.contains(12 * 60));
        let day = Window::parse("09:30-17:00").unwrap();
        assert!(day.contains(9 * 60 + 30) && !day.contains(17 * 60) && !day.contains(9 * 60));
        assert!(Window::parse("25:00-07:00").is_err());
        assert!(Window::parse("07:00-07:00").is_err());
    }

    #[test]
    fn config_example() {
        let c: Config = toml::from_str(
            r#"
[machines.thor]
mac = "fc:9d:05:67:a8:a8"
relay = "magnus@loke"

[sleep]
auto = true
window = "23:00-07:00"
"#,
        )
        .unwrap();
        assert_eq!(c.machines["thor"].relay.as_deref(), Some("magnus@loke"));
        let s = c.sleep.unwrap();
        assert_eq!(
            (
                s.command.as_str(),
                s.grace.as_str(),
                s.auto,
                s.require_locked
            ),
            ("systemctl suspend", "5m", true, true)
        );
    }
}
