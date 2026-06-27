# Veracage — Detailed Design

A wrapper that mounts a VeraCrypt volume into a private mount namespace and runs viewer apps inside a bubblewrap sandbox with explicit, user-driven clipboard and file transfer to the host.

Name: **Veracage** = VeraCrypt + `cage` (the nested-compositor fallback) — the two pillars of the isolation story.

---

## 1. Requirements (recap)

### 1.1 Threat model — defend against

- **1.1.1** Unprivileged host processes as the same user reading vault contents (indexers, backups, user-session malware)
- **1.1.2** Accidental leaks: thumbnailers, recent-files, `~/.cache`, swap of plaintext, host clipboard managers scraping sandbox clipboard

### 1.2 Functional

- **F1** Mount/dismount a VeraCrypt volume with password prompt
- **F2** Launch Kate (text), Okular (PDF), and Dolphin (file browser) inside the sandbox
- **F3** Multiple apps share one decrypted-vault session
- **F4** Clean teardown on normal exit, crash, SIGKILL, host suspend
- **F5** Read-write access to the vault from sandboxed apps
- **F6** Explicit, user-triggered clipboard transfer in both directions
- **F7** Explicit, user-triggered file transfer in both directions

### 1.3 Non-functional

- **N1** No custom crypto — VeraCrypt only
- **N2** No long-lived root daemon
- **N3** Apps render correctly: fonts, themes, GPU on opt-in
- **N4** Stock Wayland desktop; better isolation if `wp-security-context-v1` available

### 1.4 One-line summary

> The decrypted volume is unreadable to any host process outside the sandbox. Files and clipboard cross the sandbox boundary **only** on explicit user action.

---

## 2. Architecture overview

```
                    ┌─────────────────────── HOST (user session) ───────────────────────┐
                    │                                                                   │
   user ───► veracage open vault.vc                                                     │
                    │                                                                   │
                    ▼                                                                   │
            systemd --user transient scope (crash-safe cgroup)                          │
                    │                                                                   │
              ┌─────┼─────────────┬───────────────────────────────┐                     │
              │     │             │                               │                     │
              ▼     ▼             ▼                               ▼                     │
    veracage-helper  veracage-agent (tray)              ┌──── PRIVATE MOUNT NS ────┐    │
    (polkit, brief)  ├─ clipboard bridge                │                          │    │
        │            ├─ file drop zone                  │   /vault   (rw, nodev,   │    │
        ▼            └─ hotkey listener                 │            nosuid)       │    │
   unshare -m,                                          │      │                   │    │
   veracrypt mount                                      │      ▼                   │    │
                                                        │   bwrap                  │    │
                                                        │   (--unshare-pid/net/    │    │
                                                        │    ipc/uts/cgroup)       │    │
                                                        │      │                   │    │
                                                        │      ├─ kate             │    │
                                                        │      └─ okular           │    │
                                                        └──────────────────────────┘    │
                    └───────────────────────────────────────────────────────────────────┘
```

Three host-side pieces: launcher (transient), privileged helper (seconds), persistent agent. One sandbox per session, holding all running vault apps.

---

## 3. Mount namespace setup

The mount must be invisible to the host **from the moment of decryption** — not after a later "unmount from host" step. Order:

```bash
# Inside the launcher (calls helper via polkit for the privileged step)
unshare --mount --propagation=slave \
  bash -c '
    mount --make-rslave /
    veracrypt --text --mount "$VOL" /run/veracage/$$
    exec "$@"
  ' -- veracage-stage2 "$@"
```

Steps:

1. **`unshare --mount --propagation=slave`** — new mount namespace, propagation set to `slave` so we receive host mount events but never propagate ours back.
2. **`mount --make-rslave /`** — belt-and-suspenders, ensures every existing mount in the new NS is slave.
3. **`veracrypt --text --mount`** — VeraCrypt creates `/dev/mapper/veracrypt<N>` (kernel-global, unavoidable) and mounts at `/run/veracage/<pid>`. The mountpoint exists **only inside this NS**.
4. The launcher's continuation runs `bwrap` against `/run/veracage/<pid>`.

**No bind-mount-then-unmount-on-host dance.** The mount never appears on the host's `/proc/mounts`.

The dm-crypt device (`/dev/mapper/veracryptN`) remains visible to root only. Documented limitation; nothing to do about it without kernel changes.

---

## 4. Sandbox configuration (bwrap)

```bash
bwrap \
  --unshare-pid --unshare-uts --unshare-ipc --unshare-cgroup \
  --unshare-net \
  --die-with-parent --new-session \
  --proc /proc \
  --dev /dev \
  --tmpfs /tmp --tmpfs /run \
  --ro-bind /usr /usr \
  --ro-bind /etc/ld.so.cache /etc/ld.so.cache \
  --ro-bind /etc/fonts /etc/fonts \
  --ro-bind /etc/localtime /etc/localtime \
  --ro-bind /usr/share/fonts /usr/share/fonts \
  --ro-bind /usr/share/icons /usr/share/icons \
  --bind /run/veracage/$$/vault /vault \
  --remount-ro-or-flags /vault nodev,nosuid \
  --bind $XDG_RUNTIME_DIR/veracage-wayland /run/user/$UID/wayland-0 \
  --setenv WAYLAND_DISPLAY wayland-0 \
  --setenv XDG_RUNTIME_DIR /run/user/$UID \
  --setenv HOME /vault \
  --setenv XDG_DATA_HOME /vault/.config/share \
  --setenv XDG_CONFIG_HOME /vault/.config \
  --setenv XDG_CACHE_HOME /vault/.cache \
  --chdir /vault \
  -- kate /vault/notes.md
```

Key choices:

- **`--unshare-pid`**: app sees only its own PID tree; `/proc` is fresh.
- **`--unshare-net`**: zero network. No opt-in for v1; document as a non-goal until use case appears.
- **`--die-with-parent`**: launcher death tears down the sandbox.
- **`/vault` mounted `nodev,nosuid`** — `noexec` deferred (Okular/Kate don't need it, but a future shell app would).
- **`HOME = /vault`** — app config (e.g. Kate's session, Okular's bookmarks) lives inside the encrypted volume, not in `~/.config` on the host. Also satisfies 1.1.2 (no `~/.cache` plaintext leak).
- **No host home, no `/run/user/$UID/dbus-*`, no portals** — sandbox is entirely cut off from the user session except for the single Wayland socket.

### 4.1 GPU (`/dev/dri`)

Off by default. Okular renders fine on CPU for typical PDFs. Per-volume opt-in via `gpu = true` in config; documents the side-channel implication in `/etc/veracage/README`.

---

## 5. Wayland isolation

> **Decision (2026-06): Mode B only.** Veracage uses a nested `weston` (§5.2)
> on every compositor. Mode A (`wp-security-context-v1`, §5.1) was evaluated
> and **not** adopted: per the protocol definition and the KWin/wlroots
> implementations, security-context-v1 only lets the compositor *deny a
> sandbox privileged globals* and *tag its identity* — it does **not**
> partition the `wl_data_device` clipboard. The clipboard-isolation goal
> (req 1.1.2) is delivered by Mode B's **separate compositor instance**, not
> by Mode A. (GNOME/Mutter doesn't implement the protocol at all.) Mode A
> would trade clipboard isolation for window-integration UX — the wrong trade
> for this threat model. The full Mode A implementation spec is preserved in
> `docs/mode-a-security-context.md` should the trade ever change.

### 5.1 Mode A — `wp-security-context-v1` (evaluated, not adopted)

The launcher:

1. Connects to host Wayland as a regular client.
2. Binds `wp_security_context_manager_v1`.
3. Calls `create_listener()` → receives a new socket fd.
4. Sets `engine="bubblewrap"`, `app_id="org.kde.kate"`, `instance_id=<uuid>`.
5. Commits.
6. Places the new socket at `$XDG_RUNTIME_DIR/veracage-wayland`.
7. Bind-mounts that socket into the sandbox at `/run/user/$UID/wayland-0`.

Compositor effects (KWin ≥ 6, sway ≥ 1.9; **not** Mutter/GNOME, which
doesn't implement it):

- The compositor can **deny the sandbox privileged globals** —
  `wlr-data-control`, screencopy, virtual-keyboard/pointer, layer-shell — and
  **tag** its identity. This stops a clipboard manager running *inside* the
  sandbox, and stops the sandboxed app from screen-grabbing / synthesising
  input.
- **It does NOT, by protocol, make the sandbox and host clipboards
  independent.** The `wl_data_device` selection is still shared, so a *host*
  clipboard manager could read what the sandbox copies. Closing req 1.1.2 for
  clipboard needs the **separate compositor** of Mode B. *(This corrects an
  earlier version of this section, which wrongly claimed Mode A closes 1.1.2.)*

### 5.2 Mode B — nested `weston` (fallback, default on Plasma 5)

If the compositor doesn't advertise `wp-security-context-v1` (e.g. KWin 5.27 on Debian 12):

- Launcher spawns `weston --backend=wayland-backend.so --socket=veracage-N -- /usr/libexec/veracage/inner-launcher`.
- Weston nests inside a host window and includes a basic stacking WM, so multiple sandbox apps (Kate, Okular, Dolphin) can have their own windows inside the same nested compositor.
- Sandbox clipboard is fully separate from the host (different compositor instance) — closes req 1.1.2 for clipboard without needing security-context-v1.
- UX cost: all sandbox windows live inside one host window (one entry in the host taskbar). Window mixing with host apps is by alt-tab into that host window.
- (Cage was considered but it's a single-window kiosk compositor — doesn't fit F3's multi-app session.)

### 5.3 Mode C — refuse

If neither is available (unusual today), `veracage` exits with a clear error pointing the user at compositor upgrade options.

---

## 6. Clipboard bridge (F6)

Sandbox clipboard ≠ host clipboard. Transfer is explicit and one-shot.

**Components:**

- `veracage-agent` runs on the host, registers two global hotkeys via the desktop's hotkey daemon (KWin global shortcuts / GNOME settings / sway bindsym, configurable).
- The agent has access to **both** the host Wayland socket and the security-context-tagged socket (which it created in §5.1).

**Hotkeys (configurable):**

| Action | Default | Effect |
|---|---|---|
| Push host clipboard → sandbox | Super+Shift+V | Reads host clipboard, writes it to the sandbox-scope clipboard |
| Pull sandbox clipboard → host | Super+Shift+C | Reads sandbox-scope clipboard, writes it to the host clipboard |

**Mechanism (Mode A):**

```bash
# push
WAYLAND_DISPLAY=wayland-0     wl-paste --no-newline | \
WAYLAND_DISPLAY=veracage-wayland wl-copy

# pull
WAYLAND_DISPLAY=veracage-wayland wl-paste --no-newline | \
WAYLAND_DISPLAY=wayland-0     wl-copy
```

**Mechanism (Mode B, nested cage):**

Same pattern, with `WAYLAND_DISPLAY` pointing at the cage compositor's socket instead of the security-context one.

**No background bridge.** The agent does **not** mirror clipboards; each transfer is a discrete action triggered by the user. This satisfies F6 ("only on explicit user action").

**MIME types:** v1 supports text only. Image/HTML/files deferred — most leaks come from text anyway, and supporting binary clipboard cleanly across scopes adds complexity (multi-target offers, large-buffer streaming).

---

## 7. File transfer (F7)

The host filesystem is **not** mounted in the sandbox. Transfer is mediated by two staging directories **inside the vault**:

```
/vault/.veracage/in/    # files imported from host (host writes, sandbox reads)
/vault/.veracage/out/   # files exported from sandbox (sandbox writes, host reads)
```

### 7.1 Host → sandbox (import)

- The agent shows a small **drop-zone window** on the host (toggle from tray icon).
- User drags a host file onto it.
- Agent copies the file (via the host's view of the mount namespace — but the agent runs **inside** the launcher's mount NS, so it can see `/run/veracage/<pid>/vault/.veracage/in/`) into the inbox.
- File is owned by the user, mode 0600.

### 7.2 Sandbox → host (export)

- Sandboxed app saves a file to `/vault/.veracage/out/`.
- The agent watches that directory (`inotify`) and updates an "outbox" list in its tray window.
- User clicks an outbox entry → host file dialog (`xdg-portal` from the host side) → agent moves the file from `out/` to the chosen host path.

### 7.3 Why staging dirs inside the vault

- Both sides already see the vault — no third bind mount needed.
- Files in `in/` and `out/` are encrypted at rest with the rest of the vault.
- Cross-session: `in/` files persist; user clears them manually or via a "wipe inbox" tray action.

### 7.4 What this does **not** do

- No FUSE bridge, no mediated host-FS read from inside the sandbox.
- No `xdg-desktop-portal` exposed to sandboxed apps. Apps' "Open" dialogs see only the vault.

---

## 8. Lifecycle

### 8.1 Startup sequence

```
veracage open ~/Documents/work.vc
  │
  1. systemd-run --user --scope \
        --property=ExecStopPost=/usr/libexec/veracage/cleanup \
        veracage-session ~/Documents/work.vc
  │
  2. password prompt (host-side, e.g. ksshaskpass / systemd-ask-password)
  │
  3. polkit-authenticated call to veracage-helper:
       unshare -m, make-rslave /, veracrypt mount, write status, exit
  │
  4. veracage-agent starts (host scope, persistent for the session)
       - registers hotkeys
       - opens tray icon
       - creates security-context-tagged Wayland socket
  │
  5. bwrap launches with HOME=/vault and entry app (kate or okular)
  │
  6. veracage-session waits for bwrap exit; agent stays up until session ends
```

### 8.2 Multi-app launch

Subsequent launches of Kate/Okular against the same vault re-use the existing session:

```
veracage open ~/Documents/work.vc okular notes.pdf
```

The launcher detects an active session for that volume (via a per-vault lock file in `$XDG_RUNTIME_DIR/veracage/<vault-hash>.lock`) and:

- Joins the existing mount namespace with `nsenter --target $LAUNCHER_PID --mount`
- Spawns a new bwrap inside the same mount NS
- Reuses the same Wayland security context (or spawns a sibling-tagged context with a new instance_id)

### 8.3 Crash safety

- The transient scope cgroup tracks **all** descendants (launcher, agent, bwrap, helper).
- `ExecStopPost=/usr/libexec/veracage/cleanup` runs on **any** termination including SIGKILL.
- `cleanup` does:
  1. `veracrypt --dismount` (idempotent)
  2. Remove `$XDG_RUNTIME_DIR/veracage/*` for this session
  3. Tear down the security-context socket
- `--die-with-parent` on bwrap ensures sandbox dies with the launcher, even if cleanup races.

### 8.4 Suspend handling

- `veracage-agent` subscribes to `org.freedesktop.login1.Manager.PrepareForSleep`.
- On `(true)` (about to suspend): kill bwrap, signal launcher, dismount.
- User re-enters password on resume. (No keep-key-alive option; protects against sleep-attack threats not formally in scope but cheap to do right.)

---

## 9. Privilege model

**User-level launcher + polkit-authenticated helper. No setuid, no daemon.**

### 9.1 What needs root

Only:
- `mount` of dm-crypt block device (`veracrypt --text --mount`)
- `umount` / `veracrypt --dismount`
- `unshare --mount` (with `CAP_SYS_ADMIN` — actually available in user namespaces, but VeraCrypt itself needs real root for dm-setup)

### 9.2 Helper

A small (~100 LOC) binary `veracage-helper`:

- Validates the volume path against a config-defined allowlist (e.g. `~/Documents/*.vc`)
- Calls `veracrypt --text --mount` (or `--dismount`)
- Writes status to a stdout-passed pipe
- Exits

Authorized by polkit:

```xml
<action id="org.veracage.mount">
  <message>Authentication required to mount an encrypted vault</message>
  <defaults>
    <allow_active>auth_self_keep</allow_active>
  </defaults>
  <annotate key="org.freedesktop.policykit.exec.path">
    /usr/libexec/veracage/veracage-helper</annotate>
</action>
```

`auth_self_keep` lets one password prompt cover both mount and dismount within ~5 minutes.

### 9.3 What does **not** need root

- bwrap (uses user namespaces)
- The agent (pure user-session)
- Wayland security context binding (regular client)
- The clipboard bridge (`wl-copy` / `wl-paste`)

---

## 10. Configuration

`~/.config/veracage/config.toml`:

```toml
[default]
allowed_apps   = ["kate", "okular", "dolphin"]
gpu            = false                # /dev/dri pass-through
suspend_action = "dismount"           # or "ignore"

[hotkeys]
push_clipboard_to_sandbox   = "Super+Shift+V"
pull_clipboard_from_sandbox = "Super+Shift+C"
toggle_drop_zone            = "Super+Shift+D"

[volumes."~/Documents/work.vc"]
display_name = "Work"
default_app  = "okular"

[volumes."~/Documents/personal.vc"]
display_name = "Personal"
gpu          = true                   # opt-in for this vault
```

Per-volume settings inherit from `[default]`.

---

## 11. Component breakdown

| Component | Est. LOC | Privilege | Lifetime |
|---|---|---|---|
| `veracage` CLI launcher | ~300 | user | per session |
| `veracage-helper` (mount/dismount) | ~100 | root via polkit | seconds |
| `veracage-agent` (tray, bridges, drop zone) | ~600 | user | per session |
| `veracage-cleanup` (ExecStopPost) | ~50 | root via polkit | seconds |
| polkit policy XML | ~30 | n/a | static |
| systemd user unit template | ~20 | n/a | static |

Total: ~1100 LOC + config. Suggested implementation: Python 3.11+ for launcher/agent, small C or Rust binary for helper (auditable).

---

## 12. Dependencies

- `bubblewrap` ≥ 0.8
- `veracrypt` (CLI)
- `systemd` (user transient scopes, login1 D-Bus)
- `polkit`
- `wl-clipboard` (`wl-copy`, `wl-paste`)
- `python3` ≥ 3.11 (or Rust)
- For Mode B fallback: `weston` (with the wayland-backend)
- Bundled apps: `kate`, `okular`, `dolphin` (KDE Frameworks)
- Compositor: KWin ≥ 6.0, Mutter ≥ 47, sway ≥ 1.10 (any one — Mode A) **OR** any Wayland compositor (Mode B)

---

## 13. Limitations and non-goals

- **Root on host can read everything.** The dm-crypt device is in the kernel global namespace.
- **No network from sandbox**, even on opt-in. Out of scope for v1.
- **Clipboard text only** in v1 (no images, no file lists).
- **No X11 fallback.** Wayland-only.
- **No multi-user / shared-vault scenarios.** Single user, single workstation.
- **No persistent sandbox state outside the vault.** App caches are inside the vault by design (HOME=/vault). Vault gets bigger; that's the trade.

---

## 14. Open questions

1. **Helper language**: Python (matches launcher) or small auditable C/Rust binary? Lean toward **C/Rust** for the privilege boundary.
2. **Drop-zone UI toolkit**: GTK (matches GNOME) or Qt (matches KDE; matches our chosen apps Kate/Okular)? Lean **Qt**.
3. **Clipboard bridge MIME support**: text-only v1, or include `image/png` for screenshots? Defer.
4. **Vault auto-lock on idle**: detect N minutes idle and dismount? Probably yes, configurable.
5. **Desktop integration**: a `.desktop` file with `MimeType=application/x-veracage-volume` so right-click "Open" works? Likely yes, post-v1.
