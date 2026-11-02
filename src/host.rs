//! What this machine is doing: Herdr agents and screen lock.

use std::collections::HashMap;
use std::process::Command;

use serde_json::Value;

/// How many Herdr agents are working. Herdr not running counts as none;
/// agents that are idle or waiting for input count as done.
pub fn working_agents() -> usize {
    let bin = std::env::var("HERDR_BIN_PATH").unwrap_or_else(|_| "herdr".into());
    let Ok(out) = Command::new(bin).args(["agent", "list"]).output() else {
        return 0;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&out.stdout) else {
        return 0;
    };
    v["result"]["agents"]
        .as_array()
        .map(|a| a.iter().filter(|x| x["agent_status"] == "working").count())
        .unwrap_or(0)
}

fn properties(out: &[u8]) -> HashMap<String, String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// Whether every graphical session of this user is locked. No graphical
/// session (a headless machine, or logind missing) counts as locked.
pub fn locked() -> bool {
    let user = std::env::var("USER").unwrap_or_default();
    let Ok(out) = Command::new("loginctl")
        .args(["show-user", &user, "-p", "Sessions", "--value"])
        .output()
    else {
        return true;
    };
    let ids = String::from_utf8_lossy(&out.stdout).into_owned();
    for id in ids.split_whitespace() {
        let Ok(out) = Command::new("loginctl")
            .args([
                "show-session",
                id,
                "-p",
                "Type",
                "-p",
                "Class",
                "-p",
                "State",
                "-p",
                "LockedHint",
            ])
            .output()
        else {
            continue;
        };
        let p = properties(&out.stdout);
        let get = |k: &str| p.get(k).map(String::as_str).unwrap_or_default();
        let graphical = matches!(get("Type"), "x11" | "wayland" | "mir");
        if graphical
            && get("Class") == "user"
            && get("State") != "closing"
            && get("LockedHint") != "yes"
        {
            return false;
        }
    }
    true
}

pub fn hostname() -> String {
    Command::new("uname")
        .arg("-n")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
        .and_then(|h| h.split('.').next().map(String::from))
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this machine".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_loginctl_properties() {
        let p = properties(b"Type=wayland\nClass=user\nLockedHint=no\n");
        assert_eq!(p["Type"], "wayland");
        assert_eq!(p["LockedHint"], "no");
    }
}
