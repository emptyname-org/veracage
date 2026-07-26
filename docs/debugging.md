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
| Session leader (app launches, volumes) | its systemd unit's journal | `journalctl --user -u 'veracage-*' -f` |
| Sandboxed apps' own output | same journal, only while `debug = true` | as above |
| Broker (`veracage-agent`: mount flow) | its journal entry | `journalctl --user -f` |
| Compositor (rendering) | `/run/veracage/rt/compositor.log` | `tail -f /run/veracage/rt/compositor.log` |

The compositor cannot use the journal: it runs as the `veracage` uid with its
stdio swallowed by the privilege drop, so it writes its own file (mode 0644,
inside the 0711 runtime dir, so only the human can read it).

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
[+  12.0s] frames=4 submits=4 windows=1 dirty=false status=None
```

`frames` counted renders, `submits` the ones that produced damage and reached
the host: `frames` high with `submits` low means work thrown away, and both at
~60 means something is repainting continuously.

## The trade

`debug = true` lets the sandboxed apps' own stdout and stderr into the journal.
Apps print the paths of files they open, so with a volume mounted those paths
land in the user journal and stay there after the volume is closed. That is an
accidental-leak channel the threat model cares about, which is why it is off by
default and why this is a debugging switch, not a setting. Turn it off when you
are done, and `journalctl --user --vacuum-time=1d` if you were working in a
volume whose file names you would rather not keep.
