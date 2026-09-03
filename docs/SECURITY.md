# Veracage: Security model

What Veracage defends against, how, and its documented limits. Full design in
[`veracage-design.md`](veracage-design.md), the isolation mechanism in
[`uid-isolation.md`](uid-isolation.md), open findings in
[`known-problems.md`](known-problems.md).

## What it defends against

1. **Unprivileged host processes running as the same user** reading volume
   contents: indexers, backup daemons, thumbnailers, session malware.
2. **Accidental leaks**: `~/.cache` plaintext, recent-files, swap of
   plaintext, host clipboard managers (Klipper/GPaste) scraping a sandbox
   copy.

The decrypted volume is unreadable to any non-root host process outside the
sandbox. Files and clipboard cross the boundary **only** on explicit user
action.

The isolation is **one-directional**: it keeps the host out of the volume, not
the app out of the host. The app you launch is trusted, so a malicious
sandboxed app is outside the threat model (see Limitations). The sandbox
restrictions (no network, no host filesystem) exist to keep the decrypted data
from leaking out, not to confine the app.

## How

| Property | Mechanism |
|---|---|
| Volume denied to the human (and every non-root uid) | An idmapped mount presents the on-disk owner as a dedicated `veracage` system uid. Apps run as that uid. The human (even as the on-disk owner) is denied by ownership through the mount, and cannot *become* the veracage uid (needs privilege). **This is the core.** |
| Mount hidden from the host | The privileged helper mounts inside a private mount namespace (`/` made rslave), so the mount is absent from the host's `/proc/mounts` and cannot be entered from it (`/proc/<leader>/root` needs ptrace access over the veracage uid, which the human does not have). It is not *invisible*: `/proc/<pid>/mountinfo` carries no ptrace check, so a same-uid process reading the leader's copy learns that a volume is mounted, with its label, mountpoint and dm device. That is metadata, never contents (see Limitations). |
| Crashes carry no plaintext | The helper clears `coredump_filter` before it execs anything, so a core dump from the compositor, the session leader, `bwrap` or an app holds no memory at all. The setting survives `execve` and is inherited by children, so one call covers the whole session. The agent does the same for the passphrase it holds. `RLIMIT_CORE` is not the knob: the kernel ignores it when `kernel.core_pattern` is a pipe, which is the systemd default. |
| Passphrase not left in RAM | The agent holds the typed passphrase in a `Zeroizing` buffer and pipes it to the CLI, which never reads it (systemd-run `--pipe` hands the pipe straight to the helper's stdin). The root helper reads it once for `cryptsetup` and then overwrites the bytes with `write_volatile`, in the child that opens the volume and in the parent that outlives the whole session. Nothing holds a plaintext passphrase while a volume is open. |
| Block device sealed | `/dev/mapper/veracage-*` is `root:disk 0660` + `UDISKS_IGNORE=1`: no unprivileged `open`, and no desktop "mount this drive" path. `57-veracage.rules` also sets `DM_UDEV_DISABLE_DISK_RULES_FLAG`, so udev never probes the decrypted filesystem into `/dev/disk/by-label/<label>` and `/dev/disk/by-uuid/<uuid>`, which would publish the volume's identity to every local user. |
| Decrypted data has no path out to network / host FS | `bwrap` unshares pid/uts/ipc/cgroup/**net**, `--die-with-parent`, `--clearenv` + env allowlist, `HOME=/vaults`, host `$XDG_RUNTIME_DIR` hidden, with only the compositor's Wayland socket bound in. This keeps the volume data from leaking out. It protects the data, it is not a cage on the app (a malicious app is outside the threat model). |
| Minimal host surface | `/usr` read-only, **curated** `/etc` (linker, fontconfig, tz, NSS, machine-id, TLS) instead of all of `/etc`, no host home, no HOST D-Bus (each app gets a private `dbus-run-session` bus
inside its own sandbox, so Qt/KDE apps do not stall on a missing bus), no
portals. The GPU exception: `/dev/dri` plus `/sys/dev/char` and `/sys/devices` read-only, which Mesa needs to pick the hardware driver. That exposes host *device metadata* (NIC addresses, DMI ids, the device tree) to the app, which the one-directional model accepts: it is not volume data, and a malicious app is outside the threat model. |
| Clipboard isolation | The sandbox runs against the project's own nested `veracage-compositor`, which owns the selection. **No `data-control` global** is exposed to apps, so a clipboard manager inside the sandbox can't scrape it. Host<->sandbox transfer is one-shot, user-triggered (Ctrl+Alt+V/C or the toolbar), text-only. After a Copy out, the host clipboard is auto-cleared after a timeout (default 30 seconds) and again on exit, so a copied secret does not linger on the host. |
| Control socket carries no execution | The human-owned control socket exposes `ping`/`list`/`close`/`set-apps`: deliberately no command execution and no file transfer, because any same-uid process can reach it. `set-apps` only replaces the enabled-app list, the same human-trust data as `config.toml`. |
| File transfer confined to the shared directory | Host<->volume transfer goes through `~/Veracage/Exchange`, idmap-mounted at `/exchange`: a separate mount from the volume, `nosuid,nodev,noexec`, path validated by the helper. Only what the user consciously placed there is exposed. |
| Crash-safe teardown | The session runs in a `systemd --user` transient **service**. Its `ExecStopPost` runs `pkexec veracage-cleanup` (bounded: validated root-owned lock + `veracage-*` device only, `PKEXEC_UID`-owner-checked) to `cryptsetup close` every device on SIGKILL/OOM/panic/logout. |
| Suspend | A static root `/usr/lib/systemd/system-sleep/` hook `cryptsetup close`s every session before sleep. systemd blocks the transition until it returns (no D-Bus inhibitor to fail). Honors `suspend_action`. |

## Privilege model

No setuid. The only root steps (mount/dismount and the compositor spawn) run in
a small polkit-authorised Rust helper (`helper-rs/`). The compositor spawn
`exec`s and is gone. A mount **forks**: the child mounts and execs the leader in
seconds, and the parent **stays root as long as the session does**, blocked in
`waitpid`. It listens on nothing and reads no input, so after the fork nothing
can ask it to act. It is there to close every volume in the session lock when
the leader exits, which is what tears a session down when the leader is
SIGKILLed (the `ExecStopPost` below covers the case where the helper is gone
too).

**The helper does not trust its caller.** It is reachable by any active local
user via pkexec (`auth_self_keep`), so:

- the target uid/gid come from **`PKEXEC_UID`** (never argv), and it refuses
  if unset or 0.
- the continuation it `exec`s is **pinned at build time** to `{_leader,
  _compositor}`, not a caller argument.
- forwarded env vars are **allowlisted** (no `LD_PRELOAD` /
  `LD_LIBRARY_PATH` smuggling).
- it rejects unknown arguments. There is no caller-supplied mountpoint at all:
  the helper derives it as `<workspace>/<label>` from the volume's own label,
  through `sanitize_label`, which reduces the label to one safe path component.
- the `--session` id is **bound to `PKEXEC_UID`** (it is the caller's own uid),
  so a second local user can't `setns`/mount/close against another user's
  session by passing a foreign session id. `veracage-cleanup` enforces the
  same owner check.

`cargo test` and `tests/integration/test_helper_security.py` guard this
contract.

## Limitations and non-goals

- **Root on the host reads everything.** The dm-crypt device is kernel-global
  (the kernel doesn't namespace device-mapper). Deny-by-uid stores the human's
  *own* data. Inherent: a normal VM doesn't seal host root either.
- **Memory can still reach the disk through swap.** The temporary workspace and
  every app's config/cache/data live in tmpfs, and the kernel may swap tmpfs pages
  out; so may the decrypted volume's page cache. Veracage removes the *file*
  channels (`~/.cache`, recent-files, thumbnails) but cannot stop the kernel
  swapping. Use encrypted swap or zram if that matters to you.
- **Which volume is open is not a secret, only what is in it.** A same-uid
  process can learn that from several host surfaces Veracage does not own:
  `/proc/<leader-pid>/mountinfo` (label, mountpoint, dm device),
  `/sys/block/loopN/loop/backing_file` and the helper's `/proc/<pid>/cmdline`
  (the volume's path), polkit's own journal line for the mount, and the recent
  files the volume picker leaves behind (`known-problems.md`). Veracage avoids
  adding to that where it can: the dm device is named at random, the transient
  unit and its description carry the same hash instead of the filename, and
  `57-veracage.rules` keeps the decrypted filesystem's label and UUID out of
  `/dev/disk`. The kernel and polkit surfaces are not ours to close.
- **A file opened from the sandboxed file manager has its path in `/proc` while
  the app runs.** Double-clicking a file makes the file manager spawn the
  handler app inside the sandbox with the path in its argv, and
  `/proc/<pid>/cmdline` is world-readable no matter which uid owns the process,
  which `bwrap --unshare-pid` does not change (the host still sees the process
  in its own `/proc`). So the name of the file being edited is readable for as
  long as the app is open. It leaves no record: it dies with the process, and
  nothing on the host stores it. Apps started from the Apps menu carry no file
  path at all. Inherent to handing a path to a program that takes a path.
- **`debug = true` keeps app output.** Toolkits print file paths in their
  warnings, so with a volume open that output names its contents. The leader
  writes it to `apps.log` in the session's runtime scratch (0700, veracage-owned,
  on tmpfs, removed with the session), never to its own stderr: that stderr is
  the stream `pkexec` hands journald, and inheriting it put volume file names in
  the SYSTEM journal, where they survived the close and were readable by anyone
  in `adm`. Fixed after v0.7.0; a journal written by an earlier build still holds
  them (`docs/debugging.md`). Off by default either way.
- **No network** from the sandbox, even opt-in (v1 non-goal).
- **Clipboard is text-only** in v1.
- **Apps and the compositor render on the host GPU** (`/dev/dri` is passed
  through together with the `/sys/dev/char` and `/sys/devices` metadata Mesa
  reads to identify the hardware, and the `veracage` user is in the `render`
  group). The final window pixels already reach the host compositor's GPU path
  for display, so this opens no new exfiltration channel, and confining the app
  is not a goal. The `/sys` binds do let the app read host device metadata
  (see the "Minimal host surface" row).
- **A malicious *sandboxed app* is outside the threat model.** The isolation
  is one-directional (host out of the volume, not the app out of the host) and
  the app you chose to run is trusted. What stays in scope is attacker-controlled
  *data* crossing the boundary (a hostile volume's contents reaching the
  compositor, and the exchange path), reviewed as such.
- **Config integrity is load-bearing.** A same-uid process that rewrites
  `~/.config/veracage/config.toml` can redirect what launches. The sandbox is
  the barrier, not the config (see `known-problems.md`).
- **Wayland only, single user, single workstation. Cross-volume isolation is
  not a goal** (co-hosted volumes share one clipboard by design).

## Reporting

Open an issue in the tracker.
