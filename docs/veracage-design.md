# Veracage — Design

Veracage mounts an encrypted volume (LUKS or VeraCrypt) and runs viewer apps
against it in a sandbox, so the decrypted contents stay unreadable to the rest of
the host. Files and clipboard cross the boundary **only** on explicit user action.

> **Companion docs.** The isolation mechanism (deny-by-uid + the exact idmap
> recipe) and the process topology live in [`uid-isolation.md`](uid-isolation.md);
> the planned multi-volume workspace in
> [`shared-workspace-redesign.md`](shared-workspace-redesign.md); the evaluated,
> not-adopted `wp_security_context_v1` path in
> [`mode-a-security-context.md`](mode-a-security-context.md); the reader-facing
> security summary in [`SECURITY.md`](SECURITY.md); and security findings
> (resolved / open) in [`fixed-problems.md`](fixed-problems.md) /
> [`known-problems.md`](known-problems.md).

---

## 1. Requirements

**Threat model — defend against:** unprivileged host processes running **as the
same user** reading vault contents (indexers, backups, session malware); accidental
leaks (thumbnailers, recent-files, `~/.cache` plaintext, swap, host clipboard
managers scraping a sandbox copy). **Non-goals:** local root (reads everything —
inherent), cross-vault isolation (all open vaults are one trust domain), network
from the sandbox, X11, multi-user. Full analysis in `uid-isolation.md`.

**Functional:** F1 mount/dismount a LUKS or VeraCrypt volume with a passphrase
prompt · F2 launch any user-enabled app (Kate/Okular/Dolphin/…) in the sandbox ·
F3 multiple apps and multiple vaults share one compositor session · F4 clean
teardown on exit, crash, SIGKILL, suspend · F5 read-write vault access · F6
explicit, user-triggered clipboard transfer both directions · F7 explicit,
user-triggered file transfer both directions (deferred; see §5).

---

## 2. Isolation — three layers

1. **Deny (idmap).** An idmapped mount presents the vault's on-disk owner as a
   dedicated **`veracage`** system uid; apps run as that uid. The human and any
   other non-root uid are denied by ownership — even the on-disk owner, through the
   mount. Exact syscall recipe + spike evidence: `uid-isolation.md`.
2. **Hide (mount NS).** The mount lives in a private mount namespace, so it never
   appears in the host's `/proc/mounts`.
3. **Sandbox (bwrap).** Apps run under bubblewrap — `--unshare-net`, no host
   filesystem, curated `/etc`, `--clearenv` (§4).

The decrypted block device `/dev/mapper/veracage-<hex>` is `root:disk 0660` with
`UDISKS_IGNORE=1`, so there is no unprivileged path — direct `open` or a desktop
"mount this drive" click — to the plaintext.

---

## 3. Processes

Full topology in `uid-isolation.md` §"Process architecture"; in brief:

- **Human-uid launcher** (`veracage-agent`, egui): opens vaults (`pkexec`), runs
  the config picker, edits settings. The one human-side process — it does what a
  `veracage`-uid process can't (`pkexec`, host-file dialogs).
- **Root helper** (`helper-rs`, seconds, via pkexec): `cryptsetup open` →
  idmap-mount in a private NS → drop to `veracage` → exec the per-vault leader;
  also spawns the compositor on first open. Does not trust its caller (§6).
- **`veracage`-uid compositor** (`veracage-compositor`, Rust/smithay): ONE
  persistent instance; renders every vault's apps into one host (kwin) window, owns
  the clipboard, hosts the egui toolbar. Our own — not weston/cage — so the process
  facing the untrusted app is ours and the clipboard channel is private.
- **`veracage`-uid leader** (per vault): holds the mount NS, launches apps in bwrap
  wired to the compositor socket, serves a **status-only** control socket
  (`ping`/`list`/`close`).
- **CLI** (`veracage open/list/close`): the same verbs, scriptable.

---

## 4. Sandbox (bwrap)

```
bwrap --unshare-pid --unshare-uts --unshare-ipc --unshare-cgroup --unshare-net \
      --die-with-parent --new-session --clearenv \
      --proc /proc --dev /dev --tmpfs /tmp --tmpfs /run \
      --ro-bind /usr /usr   (+ curated /etc: linker, fonts, tz, NSS, machine-id, TLS) \
      --bind <mountpoint> /vault                     (nodev,nosuid) \
      --bind /run/veracage/rt/wl-vc /run/user/<uid>/wayland-0 \
      --setenv HOME /vault   (XDG_* on an ephemeral /xdg tmpfs) \
      -- <app> ...
```

- No network, no host home, no D-Bus, no portals — only the one Wayland socket.
- `HOME=/vault` so app config/caches live encrypted in the vault, not in host
  `~/.cache` (this is also what closes the accidental-leak goal).
- Only the compositor's `wl-vc` socket is bound in; the control / app-launch /
  clipboard channels are not reachable from the sandbox.
- GPU (`/dev/dri`) off by default; per-volume `gpu = true` opt-in (documented side
  channel). `noexec` on `/vault` is a tracked hardening (`known-problems.md`).

---

## 5. Clipboard & files

- **Clipboard — owned by the compositor.** It owns the sandbox selection and reaches
  the host clipboard through its own winit→kwin `smithay-clipboard` connection, so
  both sides live in one process (no cross-process relay). Push (host→sandbox) /
  pull (sandbox→host) on **Ctrl+Alt+V / Ctrl+Alt+C** and the toolbar buttons.
  Text-only in v1. No `data-control` global is exposed, so a sandbox app can't snoop
  the selection in the background.
- **Files — deferred.** The earlier `/vault/.veracage/{in,out}` staging bridge over
  the control socket was **removed**: a pen test drove it (`exec`→`export`) to
  exfiltrate the whole vault as a plain same-uid process (`fixed-problems.md`).
  Host-file import/export must be rebuilt on the human-driven compositor/launcher
  path (unreachable by other-uid processes), gated by a real user action — never the
  control socket.

---

## 6. Privilege model

No setuid, no long-lived root daemon. The only root steps — mount/dismount and the
compositor spawn — are done by the small polkit-authorised `helper-rs`, which exits
in seconds. **The helper does not trust its caller:**

- target uid/gid come from **`PKEXEC_UID`** (never argv); it refuses if unset or 0;
- the continuation it `exec`s is **pinned at build time** to `{_leader,
  _compositor}`, not a caller argument;
- forwarded env is **allowlisted** (no `LD_*` smuggling);
- unknown args rejected; the mountpoint is validated to a direct child of
  `/run/veracage`.

polkit `auth_self_keep` lets one prompt cover a short window (mount + spawn). The
passwordless `veracage-cleanup` action is bounded (validated root-owned lock +
`veracage-<12hex>` device only) and `PKEXEC_UID`-owner-checked.

---

## 7. Lifecycle

- `veracage open` → `systemd-run --user` transient **service** (a service, not a
  scope — scopes reject `Exec*`, so `ExecStopPost` would never register) → pkexec
  helper. The service's `ExecStopPost` runs `pkexec veracage-cleanup --vault-hash
  <h>`, which `cryptsetup close`s the device on graceful exit, SIGKILL, OOM, panic,
  or logout.
- The **compositor** is spawned once and persists across vault open/close; a
  **leader** dies on vault-close (or when the compositor window closes), the
  compositor does not.
- **Suspend** — a static root `/usr/lib/systemd/system-sleep/veracage` hook
  dismounts every session before sleep (systemd blocks the transition until it
  returns; honors per-owner `suspend_action=ignore`). No D-Bus watcher.

---

## 8. Configuration

`~/.config/veracage/config.toml` (auto-created):

```toml
[default]
last_used_app  = "kate"
gpu            = false          # /dev/dri passthrough (side channel; off)
suspend_action = "dismount"     # or "ignore"

[apps.kate]                     # the enabled-app allowlist — any installed binary
name = "Kate"
category = "text"
exec = "kate"
args = ["/vault"]

[volumes."/path/to/work.vc"]    # optional per-volume overrides
default_app = "okular"
gpu         = true
```

Per-volume settings inherit from `[default]`. Only apps under `[apps.*]` launch;
that list is human-side UX, not a vault-side restriction. (Config integrity is
load-bearing — see the config-tamper note in `known-problems.md`.)

---

## 9. Dependencies

- **Required:** `bubblewrap`, `cryptsetup` (+ `veracrypt` for VC volumes),
  `systemd`, `polkit`, `python3` ≥ 3.11.
- **Not needed:** no weston/cage (our own compositor), no `wl-clipboard`, no Qt/GTK,
  no `python3-gi`. The agent and compositor are self-contained Rust binaries that
  link only what a desktop session already has (Mesa GL, Wayland/X11,
  `libxkbcommon.so.0`).
- **Build only:** distro `rustc` (helper, pinned to 1.63) + rustup (agent /
  compositor, modern stable). The shipped binaries carry no such requirement.

---

## 10. Limitations & non-goals

- **Local root reads everything** — the dm device is kernel-global; deny-by-uid
  stores the human's *own* data. Inherent (a normal VM doesn't seal host root
  either).
- No network from the sandbox, even opt-in (v1).
- Clipboard text-only in v1.
- GPU off by default (per-volume opt-in, documented side channel).
- A malicious *sandboxed app* is outside the primary threat model — but the
  compositor (it parses untrusted Wayland traffic) and any future file bridge are in
  scope, and are reviewed as such.
- Wayland only; single user, single workstation; cross-vault isolation is not a goal
  (co-hosted vaults share one clipboard by design).

---

> **Historical note.** The pre-2026-07 design used a nested `weston` ("Mode B") + a
> `wl-clipboard`/global-hotkey bridge + a `/vault/.veracage` file bridge + a tray
> agent, and mounted via the `veracrypt` CLI. That whole stack was replaced by the
> deny-by-uid core + our own compositor. The transition is preserved in the git
> history.
