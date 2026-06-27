# Veracage — Security model

This summarises what Veracage defends against, how, and its documented
limits. The full design is in `veracage-design.md`.

## What it defends against

1. **Unprivileged host processes running as the same user** reading vault
   contents — indexers, backup daemons, thumbnailers, session malware.
2. **Accidental leaks** — `~/.cache` plaintext, recent-files, swap of
   plaintext, host clipboard managers (Klipper/GPaste) scraping a sandbox
   copy.

The decrypted volume is unreadable to any host process outside the sandbox;
files and clipboard cross the boundary **only** on explicit user action.

## How

| Property | Mechanism |
|---|---|
| Mount invisible to the host | The privileged helper `unshare(CLONE_NEWNS)`s, makes `/` rslave, and mounts the vault **inside** that namespace. It never appears in the host's `/proc/mounts`. |
| App isolation | `bwrap` unshares pid/uts/ipc/cgroup/**net**; `--die-with-parent`; `HOME=/vault`; host `$XDG_RUNTIME_DIR` hidden behind a tmpfs with only the Wayland socket bound in. |
| Minimal host surface | `/usr` read-only; **curated** `/etc` (linker, fontconfig, tz, NSS, machine-id, XDG, TLS) instead of all of `/etc`; no host home, no D-Bus, no portals. |
| Clipboard isolation | Sandbox runs against a separate Wayland compositor (nested weston, "Mode B"); host clipboard managers can't see it. Transfer is one-shot, user-triggered. |
| Crash-safe teardown | The session runs in a `systemd --user --scope`; its `ExecStopPost` runs `pkexec veracage-cleanup`, which `cryptsetup close`s the dm-crypt device on SIGKILL/OOM/panic/logout. |
| Suspend | The agent watches login1 `PrepareForSleep` and dismounts on suspend (configurable `suspend_action`). |

## Privilege model

No setuid, no long-lived root daemon. The only root step is mount/dismount,
done by a small polkit-authorised helper that exits in seconds.

**The helper does not trust its caller.** It is reachable by any active local
user via pkexec (polkit `auth_self_keep`), so:

- the target uid/gid come from **`PKEXEC_UID`** (set by pkexec to the real
  caller), never from argv — it refuses if unset or 0;
- the continuation it `exec`s is **pinned at build time**, not a caller
  argument;
- forwarded env vars are **allowlisted** (no `LD_PRELOAD` smuggling);
- it rejects unknown arguments.

> Earlier versions accepted caller-supplied `--user` / `--continuation` and
> would drop to that uid and exec that path — letting any active user obtain a
> root shell (`--user 0 --continuation /bin/sh`). Fixed; `cargo test` and
> `tests/integration/test_helper_security.py` guard the contract.

The privilege boundary is a small Rust binary (`helper-rs/`) for auditability;
a Python reference implementation (`helpers/veracage-helper`) is the
pre-build fallback.

## Limitations and non-goals

- **Root on the host can read everything.** The dm-crypt device
  (`/dev/mapper/veracage-*`) is in the kernel-global namespace; the kernel
  does not namespace device-mapper. Documented, unavoidable without kernel
  changes.
- **No network** from the sandbox, even opt-in (v1 non-goal).
- **Clipboard is text-only** in v1.
- **GPU is off by default.** `/dev/dri` passthrough is a per-volume opt-in
  (`gpu = true`) and a documented side channel.
- **A malicious *sandboxed app* is outside the primary threat model.** The
  file bridge's staging dirs (`/vault/.veracage/{in,out}`) are followed by
  the host agent; a hostile app could plant symlinks there (a symlink/TOCTOU
  surface). Hardening this is tracked but not implemented, since the design
  defends the vault against *host* processes, not the viewer apps the user
  chose to run.
- **Wayland only.** No X11 fallback.
- **Single user, single workstation.**

## Reporting

This is a personal project; open an issue or contact the maintainer.
