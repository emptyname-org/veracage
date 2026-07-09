# Veracage — Security model

A summary of what Veracage defends against, how, and its documented limits. Full
design in [`veracage-design.md`](veracage-design.md); the isolation mechanism +
spike evidence in [`uid-isolation.md`](uid-isolation.md); resolved / open findings
in [`fixed-problems.md`](fixed-problems.md) / [`known-problems.md`](known-problems.md).

## What it defends against

1. **Unprivileged host processes running as the same user** reading vault contents
   — indexers, backup daemons, thumbnailers, session malware.
2. **Accidental leaks** — `~/.cache` plaintext, recent-files, swap of plaintext,
   host clipboard managers (Klipper/GPaste) scraping a sandbox copy.

The decrypted volume is unreadable to any non-root host process outside the sandbox;
files and clipboard cross the boundary **only** on explicit user action.

## How

| Property | Mechanism |
|---|---|
| Vault denied to the human (and every non-root uid) | An idmapped mount presents the on-disk owner as a dedicated `veracage` system uid; apps run as that uid. The human — even as the on-disk owner — is denied by ownership through the mount, and cannot *become* the vault uid (needs privilege). **This is the core.** |
| Mount invisible to the host | The privileged helper mounts inside a private mount namespace (`/` made rslave); it never appears in the host's `/proc/mounts`. |
| Block device sealed | `/dev/mapper/veracage-*` is `root:disk 0660` + `UDISKS_IGNORE=1` — no unprivileged `open`, and no desktop "mount this drive" path. |
| App isolation | `bwrap` unshares pid/uts/ipc/cgroup/**net**; `--die-with-parent`; `--clearenv` + env allowlist; `HOME=/vault`; host `$XDG_RUNTIME_DIR` hidden, with only the compositor's Wayland socket bound in. |
| Minimal host surface | `/usr` read-only; **curated** `/etc` (linker, fontconfig, tz, NSS, machine-id, TLS) instead of all of `/etc`; no host home, no D-Bus, no portals. |
| Clipboard isolation | The sandbox runs against our own nested `veracage-compositor`, which owns the selection; **no `data-control` global** is exposed to apps, so a clipboard manager inside the sandbox can't scrape it. Host↔sandbox transfer is one-shot, user-triggered (Ctrl+Alt+V/C or the toolbar), text-only. |
| Control socket carries status only | The human-owned control socket exposes `ping`/`list`/`close` — no command execution, no file transfer. (An earlier `exec`/`export` bridge there *was* an exfiltration path; a pen test read the whole vault through it, and it was removed — see `fixed-problems.md`.) |
| Crash-safe teardown | The session runs in a `systemd --user` transient **service**; its `ExecStopPost` runs `pkexec veracage-cleanup` (bounded — validated root-owned lock + `veracage-*` device only, `PKEXEC_UID`-owner-checked) to `cryptsetup close` the device on SIGKILL/OOM/panic/logout. |
| Suspend | A static root `/usr/lib/systemd/system-sleep/` hook `cryptsetup close`s every session before sleep — systemd blocks the transition until it returns (no D-Bus inhibitor to fail); honors `suspend_action`. |

## Privilege model

No setuid, no long-lived root daemon. The only root steps — mount/dismount and the
compositor spawn — run in a small polkit-authorised Rust helper (`helper-rs/`) that
exits in seconds.

**The helper does not trust its caller.** It is reachable by any active local user
via pkexec (`auth_self_keep`), so:

- the target uid/gid come from **`PKEXEC_UID`** (never argv) — it refuses if unset
  or 0;
- the continuation it `exec`s is **pinned at build time** to `{_leader,
  _compositor}`, not a caller argument;
- forwarded env vars are **allowlisted** (no `LD_PRELOAD` / `LD_LIBRARY_PATH`
  smuggling);
- it rejects unknown arguments; the mountpoint is validated to a child of
  `/run/veracage`.

> Earlier versions accepted caller-supplied `--user` / `--continuation` and would
> drop to that uid and exec that path — letting any active user obtain a root shell
> (`--user 0 --continuation /bin/sh`). Fixed; `cargo test` and
> `tests/integration/test_helper_security.py` guard the contract.

## Limitations and non-goals

- **Root on the host reads everything.** The dm-crypt device is kernel-global (the
  kernel doesn't namespace device-mapper); deny-by-uid stores the human's *own*
  data. Inherent — a normal VM doesn't seal host root either.
- **No network** from the sandbox, even opt-in (v1 non-goal).
- **Clipboard is text-only** in v1.
- **GPU is off by default.** `/dev/dri` passthrough is a per-volume opt-in and a
  documented side channel.
- **A malicious *sandboxed app* is outside the primary threat model** — but the
  compositor (it parses untrusted Wayland traffic) and any future file bridge are in
  scope and reviewed as such.
- **Config integrity is load-bearing.** A same-uid process that rewrites
  `~/.config/veracage/config.toml` can redirect what launches; the sandbox is the
  barrier, not the config (see `known-problems.md`).
- **Wayland only; single user, single workstation; cross-vault isolation is not a
  goal** (co-hosted vaults share one clipboard by design).

## Reporting

This is a personal project; open an issue or contact the maintainer.
