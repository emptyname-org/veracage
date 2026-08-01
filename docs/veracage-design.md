# Veracage - Design

Veracage mounts an encrypted volume (LUKS or VeraCrypt) and runs apps against
it in a sandbox, so the decrypted contents stay unreadable to the rest of the
host. Files and clipboard cross the boundary **only** on explicit user action.

> **Companion docs.** The isolation mechanism (deny-by-uid + the exact idmap
> recipe) and the process topology: [`uid-isolation.md`](uid-isolation.md).
> The multi-volume workspace: [`shared-workspace.md`](shared-workspace.md).
> The single-window UI and the shared directory:
> [`single-window-ux.md`](single-window-ux.md). The evaluated, not-adopted
> `wp_security_context_v1` path:
> [`mode-a-security-context.md`](mode-a-security-context.md). The security
> summary: [`SECURITY.md`](SECURITY.md). Open findings:
> [`known-problems.md`](known-problems.md).

---

## 1. Requirements

**Threat model - defend against:** unprivileged host processes running **as
the same user** reading volume contents (indexers, backups, session malware),
and accidental leaks (thumbnailers, recent-files, `~/.cache` plaintext, swap,
host clipboard managers scraping a sandbox copy). The isolation is
**one-directional**: it keeps the host out of the volume, not the app out of
the host. The app you launch is trusted, so a **malicious sandboxed app is
outside the threat model** (not a secondary threat, just outside). The sandbox
restrictions (no network, no host filesystem) are there to keep the decrypted
data from leaking out, not to confine the app. **Non-goals:** local root
(reads everything, inherent), cross-volume isolation (all open volumes are one
trust domain), network from the sandbox, X11, multi-user. Full analysis in
`uid-isolation.md`.

**Functional:** F1 mount/dismount a LUKS or VeraCrypt volume with a passphrase
prompt. F2 launch any user-enabled app in the sandbox. F3 multiple apps and
multiple volumes share one compositor session. F4 clean teardown on exit,
crash, SIGKILL, suspend. F5 read-write volume access. F6 explicit,
user-triggered clipboard transfer both directions. F7 explicit file transfer
both directions (the shared directory, §5).

---

## 2. Isolation - three layers

1. **Deny (idmap).** An idmapped mount presents the volume's on-disk owner as
   a dedicated **`veracage`** system uid. Apps run as that uid. The human and
   any other non-root uid are denied by ownership (even the on-disk owner,
   through the mount). Exact syscall recipe: `uid-isolation.md`.
2. **Hide (mount NS).** The mount lives in a private mount namespace, so it
   never appears in the host's `/proc/mounts`.
3. **Sandbox (bwrap).** Apps run under bubblewrap: `--unshare-net`, no host
   filesystem, curated `/etc`, `--clearenv` (§4).

The decrypted block device `/dev/mapper/veracage-<hex>` is `root:disk 0660`
with `UDISKS_IGNORE=1`, so there is no unprivileged path (direct `open` or a
desktop "mount this drive" click) to the plaintext.

---

## 3. Processes

Full topology in `uid-isolation.md`. In brief:

- **Human-uid broker** (`veracage-agent`, egui): mounts volumes (`pkexec`),
  runs the config picker, edits settings. The one human-side process: it does
  what a `veracage`-uid process can't (`pkexec`, host-file dialogs).
- **Root helper** (`helper-rs`, runs for seconds, via pkexec):
  `cryptsetup open` -> check the filesystem -> idmap-mount into the workspace
  namespace -> drop to `veracage` -> exec the session leader. Does not trust its
  caller (§6). The check is `fsck -p` on the decrypted device, before anything
  mounts it: a volume that was not dismounted cleanly is repaired where preen mode
  can do it unambiguously, and is otherwise left closed and dismounted with a
  message, rather than mounted dirty. ext2/3/4, FAT and exFAT are checked; a
  filesystem with no preen-capable checker installed is mounted as before.
- **`veracage`-uid compositor** (`veracage-compositor`, Rust/smithay): ONE
  persistent instance. Renders every volume's apps into one host window, owns
  the clipboard, hosts the egui menu bar. First-party (not weston/cage), so
  the compositor is under this project's control and the clipboard channel is
  private (no `data-control` global is exposed to apps).
- **`veracage`-uid session leader** (one per session): holds the workspace
  mount namespace, launches apps in bwrap wired to the compositor socket,
  serves a **status-only** control socket (`ping`/`list`/`close`).
- **CLI** (`veracage open/list/close/close-volume`): the same verbs,
  scriptable.

---

## 4. Sandbox (bwrap)

```
bwrap --unshare-pid --unshare-uts --unshare-ipc --unshare-cgroup --unshare-net \
      --die-with-parent --new-session --clearenv \
      --proc /proc --dev /dev --tmpfs /tmp --tmpfs /run \
      --ro-bind /usr /usr   (+ curated /etc: linker, fonts, tz, NSS, machine-id, TLS) \
      --bind <workspace> /vaults                     (each volume at /vaults/<label>) \
      --bind <exchange> /exchange                    (nosuid,nodev,noexec; optional) \
      --bind /run/veracage/rt/wl-vc /run/user/<uid>/wayland-0 \
      --setenv HOME /vaults   (XDG_* on an ephemeral /xdg tmpfs) \
      -- <app> ...
```

- No network, no host home, no D-Bus, no portals. Only the one Wayland
  socket.
- `HOME=/vaults` so app config/caches live encrypted in the open volumes, not
  in host `~/.cache` (this is what closes the accidental-leak goal).
- Only the compositor's `wl-vc` socket is bound in. The control / app-launch /
  clipboard channels are not reachable from the sandbox.
- GPU always passed through: `/dev/dri` plus the `/sys` device metadata Mesa
  needs to pick its hardware driver (without them apps fall back to llvmpipe
  software rendering). The `veracage` user is added to the `render` group at
  install, and the helper applies it via `initgroups` on privilege drop.
  `noexec` on the volume mounts is a tracked hardening (`known-problems.md`).

---

## 5. Clipboard & files

- **Clipboard: owned by the compositor.** It owns the sandbox selection and
  reaches the host clipboard as a wlr-data-control client on its own
  winit->host connection, so both sides live in one process (no cross-process
  relay). Paste in (host->sandbox) / Copy out (sandbox->host) on
  **Ctrl+Alt+V / Ctrl+Alt+C** and the toolbar buttons. Text-only in v1. No
  `data-control` global is exposed, so a sandbox app can't snoop the clipboard
  in the background. After a Copy out, the host clipboard is cleared
  automatically after a timeout (default 30 seconds) and again when Veracage
  quits, so a copied secret does not linger on the host.
- **Files: the shared directory.** `~/Veracage/Exchange` on the host is
  idmap-mounted into the sandbox at `/exchange`. Files dropped on either side
  appear on the other, owned by the user. Details + security analysis:
  `single-window-ux.md`. Host-file import/export *dialogs* are deferred. If
  built, they run on the human-driven compositor/broker path (unreachable by
  other-uid processes), gated by a real user action, never the control
  socket.

---

## 6. Privilege model

No setuid, no long-lived root daemon. The only root steps (mount/dismount and
the compositor spawn) are done by the small polkit-authorised `helper-rs`,
which exits in seconds. **The helper does not trust its caller:**

- target uid/gid come from **`PKEXEC_UID`** (never argv). It refuses if unset
  or 0.
- the continuation it `exec`s is **pinned at build time** to `{_leader,
  _compositor}`, not a caller argument.
- forwarded env is **allowlisted** (no `LD_*` smuggling).
- unknown args rejected. The mountpoint is validated to a direct child of
  `/run/veracage`.

polkit `auth_self_keep` lets one prompt cover a short window (mount + spawn).
The passwordless `veracage-cleanup` action is bounded (validated root-owned
lock + `veracage-<12hex>` device only) and `PKEXEC_UID`-owner-checked.

---

## 7. Lifecycle

- `veracage open` -> `systemd-run --user` transient **service** (a service,
  not a scope: scopes reject `Exec*`, so `ExecStopPost` would never register)
  -> pkexec helper. The service's `ExecStopPost` runs
  `pkexec veracage-cleanup`, which `cryptsetup close`s every open volume's
  device on graceful exit, SIGKILL, OOM, panic, or logout.
- The **compositor** is spawned once (its own `systemd --user` unit) and
  persists across sessions. The **session leader** dies on session close (or
  when the compositor window closes), the compositor does not. Individual
  volumes dismount without ending the session (`shared-workspace.md`).
- **Suspend**: a static root `/usr/lib/systemd/system-sleep/veracage` hook
  dismounts every session before sleep (systemd blocks the transition until it
  returns, and honors per-owner `suspend_action=ignore`). No D-Bus watcher.

---

## 8. Configuration

`~/.config/veracage/config.toml` (auto-created):

```toml
[default]
last_used_app  = "kate"
suspend_action = "dismount"     # dismount on suspend, or "ignore"
clip_clear     = true           # auto-clear host clipboard after Copy out
clip_clear_timeout = 30         # seconds before the auto-clear fires

[apps.kate]                     # the enabled-app allowlist (any installed binary)
name = "Kate"
exec = "kate"

[volumes."/path/to/work.vc"]    # optional per-volume overrides
default_app = "okular"
```

Per-volume settings inherit from `[default]`. Only apps under `[apps.*]`
launch. That list is human-side UX, not a volume-side restriction. (Config
integrity is load-bearing, see the config-tamper note in
`known-problems.md`.)

---

## 9. Dependencies

- **Required:** `bubblewrap`, `cryptsetup` (its `tcrypt` module opens VeraCrypt
  volumes natively, so the `veracrypt` binary is not required), `systemd`,
  `polkit`, `python3` >= 3.11.
- **Not needed:** no weston/cage (own compositor), no `wl-clipboard`, no
  Qt/GTK, no `python3-gi`. The agent and compositor are self-contained Rust
  binaries that link only what a desktop session already has (Mesa GL,
  Wayland/X11, `libxkbcommon.so.0`).
- **Build only:** distro `rustc` (helper, pinned to 1.63) + rustup (agent /
  compositor, modern stable). The shipped binaries carry no such requirement.

---

## 10. Limitations & non-goals

- **Local root reads everything**: the dm device is kernel-global.
  Deny-by-uid stores the human's *own* data. Inherent (a normal VM doesn't
  seal host root either).
- No network from the sandbox, even opt-in (v1).
- Clipboard text-only in v1.
- Apps and the compositor render on the host GPU. The final window pixels
  already reach the host compositor's GPU path for display, so this opens no
  new exfiltration channel, and confining the app is not a goal (below).
- A malicious *sandboxed app* is **outside the threat model**: the isolation is
  one-directional (host out of the volume, not the app out of the host) and the
  app you chose to run is trusted. What stays in scope is attacker-controlled
  *data* crossing the boundary (a hostile volume's filesystem label parsed by
  the leader/compositor, the exchange path, and any future file bridge),
  reviewed as such.
- Wayland only. Single user, single workstation. Cross-volume isolation is not
  a goal (co-hosted volumes share one clipboard by design).
