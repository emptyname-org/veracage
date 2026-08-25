# Debugging

Veracage runs as four processes across two uids, and the sandboxed apps have
their output discarded on purpose, so "it felt slow" is hard to chase from
outside. Turning on debug logging makes each part say what it is doing, with
timings.

## Turning it on

```toml
# ~/.config/veracage/config.toml
[default]
debug = true
```

Then quit Veracage and start it again: the flag is read when each process
starts. It reaches the compositor as `VERACAGE_DEBUG=1` (through the helper's
env allowlist) and the session leader as `--debug`.

## Where the logs go

| Component | Where | Read it with |
| --- | --- | --- |
| Session leader (app launches, volumes) | the SYSTEM journal | `journalctl _UID=$(id -u veracage) -f` |
| Sandboxed apps' own output | same, only while `debug = true` | as above |
| Broker (`veracage-agent`: mount flow) | the user journal | `journalctl --user -f` |
| Compositor (rendering) | `/run/veracage/rt/compositor.log` | `tail -f /run/veracage/rt/compositor.log` |

The compositor cannot use the journal at all: it runs as the `veracage` uid with
its stdio swallowed by the privilege drop, so it writes its own file (mode 0644,
inside the 0711 runtime dir, so only the human can read it).

The leader keeps its stderr, but that stderr does not go where its unit is:
journald splits the journal by the uid of the sender, and the leader is the
`veracage` uid, so its lines land in the SYSTEM journal even though the unit is
a `--user` one. `journalctl --user -u 'veracage-*'` shows the unit's start and
stop and none of its output. Reading the system journal needs the `adm` or
`systemd-journal` group.

## Putting the compositor log somewhere else

`/run` is a tmpfs, so that log dies with the machine, and the 0711 runtime dir
cannot be listed - only opened by name. To keep it somewhere readable instead:

```toml
# ~/.config/veracage/config.toml
[default]
debug   = true
log_dir = "/home/you/logs"    # absolute; must exist and be writable by uid veracage
```

With a log directory configured, the session leader writes its own lines to
`<log_dir>/leader.log` instead of that journal, so both logs sit side by side.
The sandboxed apps' output stays in the journal deliberately: putting it in the
file would mean handing a sandboxed app a writable fd into a host directory.

The directory is NOT created for you, and Veracage does not widen its
permissions: the compositor runs as the `veracage` uid, so the directory has to
be one that uid can write (`chmod 1777`, or owned by it). The file is opened
with `O_NOFOLLOW`, so a symlink planted in that directory is refused rather than
followed. With `log_dir` unset, or naming something that is not a directory (the
CLI says so on stderr and ignores it), the log stays at
`/run/veracage/rt/compositor.log`. A directory that exists but the `veracage`
uid cannot write is the one case that loses lines silently, because the
compositor has no stdio to complain on.

A log outside the runtime dir is readable by whoever can reach that directory.
The lines are render counters and cursor decisions, not volume contents, but the
sandboxed apps' own output (which does name paths inside the volume) goes to the
journal whenever `debug = true` - see the trade below.

## What each line means

Leader, one line per event, `+seconds` since it started:

```
veracage[+  1.24s] launch 'Dolphin': exec=/usr/bin/dolphin seeds=2 argv=71 words
veracage[+  1.28s] launch 'Dolphin': pid=12345 spawned in 38ms
veracage[+ 31.90s] exit 'Dolphin': pid=12345 status=0 after 30.6s
```

`spawned in` is the leader's own cost (building the sandbox and forking). The
gap between that and the app's window appearing is the app's own startup, which
is where the sandboxed app's output helps.

Compositor, one line per second while anything is happening:

```
[+  12.0s] frames=4 submits=4 windows=1 dirty=false mods=none status=None
```

`frames` counted renders, `submits` the ones that produced damage and reached
the host: `frames` high with `submits` low means work thrown away, and both at
~60 means something is repainting continuously. `mods` is what the seat believes
is held: anything but `none` on a line where nothing is being typed is a latched
modifier, which the sandboxed app sees as a wheel that zooms or letters that run
commands.

## The trade

`debug = true` lets the sandboxed apps' own stdout and stderr into the journal.
Apps print the paths of files they open, so with a volume mounted those paths
land in the user journal and stay there after the volume is closed. That is an
accidental-leak channel the threat model cares about, which is why it is off by
default and why this is a debugging switch, not a setting. Turn it off when you
are done, and `journalctl --user --vacuum-time=1d` if you were working in a
volume whose file names you would rather not keep.
