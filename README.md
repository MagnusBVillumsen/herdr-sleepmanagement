# herdr-sleepmanagement

`herdr-sleep` lets the machines that run your [Herdr](https://herdr.dev) agents
sleep when there is nothing to do, and wakes them again when there is.

- **Sleep when you are done**: tell a machine you are leaving, and it
  suspends once its agents, and the background commands they started, have
  finished. Nothing happens unless you ask, so a machine you are using stays
  on. It is per machine: only the agents running there count.
- **An automatic policy** (opt-in, `auto = true`) such as "sleep at night,
  when the screen is locked and nothing has happened for 10 minutes", applied
  by Herdr itself through the plugin's hooks.
- **`herdr-sleep wake`**: wake a machine with Wake-on-LAN, directly or through
  an always-on host on its LAN, and wait until it answers on SSH. Other tools
  can call it before connecting.

```
$ herdr-sleep status
thor
  policy   manual only
  pending  sleeps when agents are done (grace 5m, pid 81234)
  awake    w1:pB claude  working
           w4:p2 claude  background: cargo build --release 35% cpu

  mimir        up     runs `ssh mimir-bmc power on`
  thor         up     Wake-on-LAN fc:9d:05:67:a8:a8 via magnus@loke
```

## What counts as busy

A machine is busy while any Herdr agent is **working**, or while a command an
agent started is still using CPU. Agents often report idle while a build or
test run they started in the background is still going, so `herdr-sleep`
looks at each agent's tool shells: the `sh -c`, `bash -lc` or `zsh -c`
processes agents run commands in. If a tool shell and everything below it used
at least `busy_cpu` percent of one core (default 10) since the last check, the
machine stays awake. A dev server idling in the background does not count; a
`cargo build` does. Language servers and MCP servers an agent runs are not
tool shells and are ignored.

Agents that are idle or waiting for input (blocked) count as done: a machine
should not stay awake all night for a question you will answer in the
morning. If Herdr is not running, there are no agents.

## Install

```bash
herdr plugin install MagnusBVillumsen/herdr-sleepmanagement
```

This builds the plugin with `cargo` and does two things:

- It adds the actions **Sleep when agents are done** and **Cancel sleep**,
  which you bind to keys in `~/.config/herdr/config.toml`. This is the
  normal use: press it when you leave the machine.
- With `auto = true` it also applies the automatic policy (see below). Herdr
  starts a watcher when it starts and whenever an agent changes state; only
  one runs at a time, and it stops with Herdr. Without `auto` it does nothing
  on its own.

```toml
[[keys.command]]
key = "prefix+shift+s"
type = "plugin_action"
command = "herdr-sleep.sleep"
description = "sleep when agents are done"

[[keys.command]]
key = "prefix+shift+z"
type = "plugin_action"
command = "herdr-sleep.cancel"
description = "cancel sleep"
```

To use `wake` and `status` from a shell too, install the binary:

```bash
cargo install --git https://github.com/MagnusBVillumsen/herdr-sleepmanagement
# or
nix profile install github:MagnusBVillumsen/herdr-sleepmanagement
```

The Nix package also contains a ready-built plugin directory, which needs no
`cargo` at install time:

```bash
herdr plugin link "$(nix build --print-out-paths github:MagnusBVillumsen/herdr-sleepmanagement)/share/herdr-sleep/plugin"
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

# How this machine sleeps.
[sleep]
command = "systemctl suspend"   # default
grace = "5m"                    # how long it must be idle; default
busy_cpu = 10                   # percent of one core; default

# Only for the automatic policy; when-done ignores these.
auto = false                    # default: sleep only when asked
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
herdr-sleep watch [--hook] [--dry-run]
herdr-sleep status
```

`wake` exits 0 once the machine answers on SSH (or right away if it already
does) and 1 if it does not within the timeout. `--check` only reports, and
never wakes anything.

`when-done` sleeps the machine it runs on, and ignores `auto`, the window and
the lock: you asked for it. To send another machine to bed, run it there, e.g.
`ssh thor herdr-sleep when-done --detach`. It waits in
the foreground; `--detach` keeps it waiting in the background after the shell
exits. Only one can be pending, and `cancel` stops it. The sleep command and
`busy_cpu` come from `[sleep]` when it is there.

`watch` applies the automatic policy when `auto = true`, checking every 30
seconds; without it, the plugin's hooks exit at once. The grace
period always starts over after the machine resumes, so it does not go
straight back to sleep. `--hook` is what the plugin runs. Try a policy with
`herdr-sleep watch --dry-run` first: it prints the sleep command instead of
running it.

Durations are `30s`, `10m`, `1h30m` or a plain number of minutes.

### Without the plugin

To apply the policy from a user service instead, run `watch` there. The
service and the plugin share a lock: while one applies the policy, the other
waits.

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

## License

MIT ([LICENSE-MIT](LICENSE-MIT)) or Apache-2.0 ([LICENSE-APACHE](LICENSE-APACHE)), at your option.
