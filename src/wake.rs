use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs, UdpSocket};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Machine;

/// Parses `aa:bb:cc:dd:ee:ff` (or with `-`) into bytes.
pub fn mac(s: &str) -> Result<[u8; 6]> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 {
        bail!("bad MAC address {s:?}");
    }
    let mut out = [0; 6];
    for (b, p) in out.iter_mut().zip(parts) {
        *b = u8::from_str_radix(p, 16).with_context(|| format!("bad MAC address {s:?}"))?;
    }
    Ok(out)
}

/// Six 0xff bytes, then the MAC sixteen times.
pub fn packet(mac: [u8; 6]) -> Vec<u8> {
    let mut p = vec![0xff; 6];
    for _ in 0..16 {
        p.extend_from_slice(&mac);
    }
    p
}

fn hex(mac: [u8; 6]) -> String {
    mac.iter().map(|b| format!("{b:02x}")).collect()
}

fn send_local(mac: [u8; 6]) -> Result<()> {
    let sock = UdpSocket::bind("0.0.0.0:0").context("opening a UDP socket")?;
    sock.set_broadcast(true)?;
    let p = packet(mac);
    for _ in 0..3 {
        sock.send_to(&p, "255.255.255.255:9")
            .context("sending the magic packet")?;
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

/// Sends the packet from `relay` with python3, which needs nothing installed
/// beyond what most Linux and macOS hosts already have.
fn send_relay(relay: &str, mac: [u8; 6]) -> Result<()> {
    let script = format!(
        "import socket, time\n\
         p = b'\\xff' * 6 + bytes.fromhex('{}') * 16\n\
         s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)\n\
         s.setsockopt(socket.SOL_SOCKET, socket.SO_BROADCAST, 1)\n\
         for _ in range(3):\n    s.sendto(p, ('255.255.255.255', 9)); time.sleep(0.2)\n",
        hex(mac)
    );
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            relay,
            "python3 -",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .context("running ssh")?;
    child
        .stdin
        .take()
        .context("ssh stdin")?
        .write_all(script.as_bytes())?;
    if !child.wait()?.success() {
        bail!("sending the magic packet from {relay} failed");
    }
    Ok(())
}

/// Whether `host` answers on its SSH port within `timeout`.
pub fn reachable(host: &str, timeout: Duration) -> bool {
    let Ok(addrs) = (host, 22).to_socket_addrs() else {
        return false;
    };
    addrs
        .into_iter()
        .any(|a| TcpStream::connect_timeout(&a, timeout).is_ok())
}

pub fn host<'a>(name: &'a str, m: &'a Machine) -> &'a str {
    m.host.as_deref().unwrap_or(name)
}

/// Describes how `m` is woken, for `status` and `--dry-run`.
pub fn method(m: &Machine) -> Option<String> {
    match (&m.wake, &m.mac, &m.relay) {
        (Some(cmd), _, _) => Some(format!("runs `{cmd}`")),
        (None, Some(mac), Some(relay)) => Some(format!("Wake-on-LAN {mac} via {relay}")),
        (None, Some(mac), None) => Some(format!("Wake-on-LAN {mac}")),
        (None, None, _) => None,
    }
}

pub fn send(name: &str, m: &Machine) -> Result<()> {
    match (&m.wake, &m.mac, &m.relay) {
        (Some(cmd), _, _) => {
            let status = Command::new("sh").args(["-c", cmd]).status()?;
            if !status.success() {
                bail!("`{cmd}` failed ({status})");
            }
            Ok(())
        }
        (None, Some(addr), relay) => {
            let addr = mac(addr)?;
            match relay {
                Some(r) => send_relay(r, addr),
                None => send_local(addr),
            }
        }
        (None, None, _) => bail!("no way to wake {name}: set mac or wake under [machines.{name}]"),
    }
}

/// Polls the SSH port until it answers or `timeout` passes.
pub fn wait(host: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if reachable(host, Duration::from_secs(3)) {
            return true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet() {
        let m = mac("fc:9d:05:67:a8:a8").unwrap();
        assert_eq!(mac("FC-9D-05-67-A8-A8").unwrap(), m);
        let p = packet(m);
        assert_eq!(p.len(), 102);
        assert_eq!(&p[..6], &[0xff; 6]);
        assert_eq!(&p[6..12], &m);
        assert_eq!(&p[96..], &m);
        assert_eq!(hex(m), "fc9d0567a8a8");
        assert!(mac("fc:9d:05:67:a8").is_err());
        assert!(mac("zz:9d:05:67:a8:a8").is_err());
    }
}
