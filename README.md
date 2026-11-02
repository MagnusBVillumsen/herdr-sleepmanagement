# herdr-sleepmanagement

`herdr-sleep` lets the machines that run your [Herdr](https://herdr.dev) agents
sleep when there is nothing to do, and wakes them again when there is.

- **`when-done`**: sleep this machine once no agent has been working for a
  while. Start it before you walk away and the machine suspends after the last
  agent finishes.
- **`watch`**: a standing policy for a user service, such as "sleep at night,
  when the screen is locked and no agent has worked for 10 minutes".
- **`wake`**: wake a machine with Wake-on-LAN, directly or through an
  always-on host on its LAN, and wait until it answers on SSH. Other tools can
  call it before connecting.

```
$ herdr-sleep status
thor
  policy   sleep after 10m without working agents, 23:00-07:00, only when locked
  now      outside window · unlocked · 1 working agent(s)
  pending  nothing

  mimir        up     runs `ssh mimir-bmc power on`
  thor         up     Wake-on-LAN fc:9d:05:67:a8:a8 via magnus@loke
```

An agent counts as done when Herdr reports it idle or waiting for input
(blocked): a machine should not stay awake all night for a question you will
answer in the morning. If Herdr is not running, there are no agents.

## Install

As a Herdr plugin (builds with `cargo`):

```bash
herdr plugin install MagnusBVillumsen/herdr-sleepmanagement
```

This adds two actions, **Sleep when agents are done** and **Cancel sleep**.
Bind them in `~/.config/herdr/config.toml`:

```toml
[[keys.command]]
key = "prefix+z"
type = "plugin_action"
command = "herdr-sleep.sleep"
description = "sleep when agents are done"

[[keys.command]]
key = "prefix+Z"
type = "plugin_action"
command = "herdr-sleep.cancel"
description = "cancel sleep"
```

Or install the binary on its own:

```bash
cargo install --git https://github.com/MagnusBVillumsen/herdr-sleepmanagement
# or
nix profile install github:MagnusBVillumsen/herdr-sleepmanagement
```

## Configuration

`$XDG_CONFIG_HOME/herdr-sleep/config.toml` (usually
`~/.config/herdr-sleep/config.toml`). Every part is optional.

```toml
# Machines you can wake. The name is the host whose SSH port shows it is up.
[machines.thor]
mac = "fc:9d:05:67:a8:a8"
relay = "magnus@loke"      # send the packet from this host over SSH
# host = "thor.lan"        # wait for SSH here instead of "thor"

[machines.mimir]
wake = "ssh mimir-bmc power on"   # any shell command, instead of mac

# This machine's policy for `herdr-sleep watch`.
[sleep]
command = "systemctl suspend"   # default
grace = "10m"                   # default
window = "23:00-07:00"          # local time; leave out to allow any time
require_locked = true           # default
```

Without `relay`, the magic packet is broadcast from the machine running
`herdr-sleep`, which only works on the same LAN. With `relay`, it is sent from
that host by a short `python3` script over SSH, so the relay needs nothing
else installed. Wake-on-LAN must be enabled in the target's firmware and
network settings.

`require_locked` means every graphical session of your user is locked
(logind's `LockedHint`). A machine with no graphical session counts as locked.

## Commands

```
herdr-sleep wake <machine> [--timeout 120] [--no-wait] [--dry-run]
herdr-sleep wake --check <machine>
herdr-sleep when-done [--grace 5m] [--detach] [--dry-run]
herdr-sleep cancel
herdr-sleep watch [--dry-run]
herdr-sleep status
```

`wake` exits 0 once the machine answers on SSH (or right away if it already
does) and 1 if it does not within the timeout. `--check` only reports, and
never wakes anything.

`when-done` ignores the window and the lock: you asked for it. With
`--detach` it keeps waiting as the transient user unit
`herdr-sleep-when-done`, or as a background process where there is no systemd
user manager. Only one can be pending; `cancel` stops it.

Durations are `30s`, `10m`, `1h30m` or a plain number of minutes.

### As a user service

`watch` checks every 30 seconds. A grace period always starts over after the
machine resumes, so it does not go straight back to sleep.

```ini
# ~/.config/systemd/user/herdr-sleep.service
[Unit]
Description=Sleep when Herdr agents are done

[Service]
ExecStart=%h/.cargo/bin/herdr-sleep watch
Restart=on-failure

[Install]
WantedBy=default.target
```

```bash
systemctl --user enable --now herdr-sleep
```

Try a policy with `herdr-sleep watch --dry-run` first: it prints the sleep
command instead of running it.

## License

MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)), at your option.
