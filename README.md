# Veracage

Runs apps against an encrypted volume (LUKS or VeraCrypt) in a sandbox, so the
decrypted contents stay unreadable to the rest of the host. Full design in
[`docs/veracage-design.md`](docs/veracage-design.md); security model in
[`docs/SECURITY.md`](docs/SECURITY.md); the isolation core in
[`docs/uid-isolation.md`](docs/uid-isolation.md).

## How it works

A small root helper (via `pkexec`) `cryptsetup open`s the vault and **idmap-mounts**
it so its contents are owned by a dedicated `veracage` system uid, inside a **private
mount namespace** — so the human and every other non-root uid are denied by
ownership, and the mount never appears in `/proc/mounts`. Apps run as `veracage`
under **bubblewrap** (no network, no host filesystem, curated `/etc`, `--clearenv`),
rendered by our own persistent nested Wayland compositor (`veracage-compositor`),
which owns the clipboard and hosts an in-window toolbar.

## What works

- **One window: the compositor**, with a **File / Edit / Apps / Settings** menu bar
  in its chrome. The human-side helper (`veracage-agent`) is a **windowless broker**
  — it does the host-side things a `veracage`-uid process can't (`pkexec` the mount,
  host file dialogs, write `~/.config`) and shows only transient dialogs. The front
  door (app-menu icon, or "Open with" a `.vc`) is a volume picker + passphrase
  dialog, not a standing window. (Design: `docs/single-window-ux.md`.)
- **`veracage open <vault> [app]`** — bring up the compositor, then mount: two
  pkexecs behind a **single** password prompt (polkit `auth_self_keep`);
  launch apps from the compositor **Apps menu** (no second prompt).
- **`veracage list <vault>` / `veracage close <vault>`** — inspect / tear down.
- **Clipboard** — Ctrl+Alt+V (host→sandbox) / Ctrl+Alt+C (sandbox→host) or the
  toolbar buttons; text-only, user-triggered, owned by the compositor.
- **File transfer** — a shared **Exchange folder**: `~/Veracage/Exchange` on the
  host is idmap-mounted into the sandbox at `/exchange`. Drop a file in on either
  side and it's there on the other, owned by you — no dialogs, no copies. Off via
  `exchange = false`. (Design: `docs/single-window-ux.md`.)
- **Multi-volume shared workspace** — open several volumes at once and they share
  **one private mount namespace**, appearing side by side at `/vaults/<label>`. One
  set of apps sees them all, so a single file manager can **drag-and-drop between
  volumes**. Subsequent opens `setns` into the running session; **Close volume ▸**
  closes just one (the rest run on). Volumes are shown at an app's **launch time**
  (open them first, then launch). (Design: `docs/shared-workspace-redesign.md`.)
- **Enable any installed app** — `veracage configure` (GUI picker) or
  `--add <binary>` / `--remove <key>` / `--list`. No fixed catalog.
- **Crash-safe teardown** — the session is a `systemd --user` transient service
  whose `ExecStopPost` `cryptsetup close`s the device on any exit
  (SIGKILL/OOM/panic/logout).
- **Suspend** — a static root `system-sleep` hook dismounts every session before
  sleep (no D-Bus watcher).
- **Privilege helper in Rust** (`helper-rs/`): derives the caller uid from
  `PKEXEC_UID` (never argv), pins the continuation at build time, allowlists
  forwarded env. See `docs/SECURITY.md`.

Deferred: host-file import/export (the old socket file bridge was removed — see
`docs/fixed-problems.md`) and GlobalShortcuts-portal integration for the clipboard
keybinds. `wp_security_context_v1` (Mode A) was evaluated and **not** adopted
([`docs/mode-a-security-context.md`](docs/mode-a-security-context.md)).

## Install

Debian 12 system packages:

```
sudo apt install bubblewrap cryptsetup veracrypt python3
```

`bubblewrap` + `cryptsetup` (+ `veracrypt` for VC volumes) are required. **No
weston, no wl-clipboard, no Qt/GTK, no python3-gi** — the agent and compositor are
self-contained Rust binaries that link only what a desktop session already has
(Mesa GL, Wayland/X11, `libxkbcommon.so.0`).

### Toolchain (build only)

- The privilege **helper** builds on Debian 12's stock `rustc` 1.63.
- The **agent** and **compositor** need rustup + recent stable
  (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`); `make install`
  uses rustup's cargo for both. If `libxkbcommon-dev` is absent, `make` synthesises
  a private linker symlink to `libxkbcommon.so.0` (no `-dev` package needed).

```
make install-dev     # build the helper + point a polkit policy at this checkout
sudo make install    # system install to /usr/local (override with PREFIX=)
```

`make install` lays down the package under `PREFIX/lib/veracage`; `veracage`,
`veracage-agent`, and `veracage-compositor` in `PREFIX/bin`; the privileged helpers
in `PREFIX/libexec/veracage`; a polkit policy, a `.desktop` + icon, a udev rule
(hides the dm device from UDisks), and the `system-sleep` hook. It also creates the
`veracage` system user.

## Usage

```
veracage configure                    # enable apps (GUI picker)
veracage configure --add dolphin      # or by binary name
veracage open /path/to/vault.vc       # mount into the shared workspace
veracage open /path/to/vault.vc kate  # auto-launch an app too
veracage open /path/to/other.vc       # a 2nd volume joins the same workspace
veracage list  /path/to/vault.vc
veracage close-volume <label>         # close one volume (or Close volume ▸ menu)
veracage close /path/to/vault.vc      # tear the whole session down
```

`pkexec` prompts for your account password; the CLI then prompts for the vault
passphrase on the terminal. The GUI launcher (`veracage-agent`, or the "Veracage"
app-menu entry) collects the passphrase in a window and pipes it to `cryptsetup`.

## Config

`~/.config/veracage/config.toml` (auto-created):

```toml
[default]
last_used_app  = "kate"
gpu            = false          # /dev/dri passthrough (side channel; off)
suspend_action = "dismount"     # or "ignore" to keep mounted across suspend

[apps.kate]                     # the enabled-app allowlist (any installed binary)
name     = "Kate"
category = "text"
exec     = "kate"
args     = ["/vaults"]          # the app opens on the workspace (all open volumes)

[volumes."/home/you/Documents/work.vc"]   # optional per-volume overrides
default_app  = "okular"
gpu          = true
```

Per-volume settings inherit from `[default]`. Only apps under `[apps.*]` launch —
add one with `veracage configure --add <binary>` (there is no hardcoded catalog).

## Tests

```
make test        # Python unit suite (pytest) — 154 tests
make lint        # ruff + mypy
make test-rs     # Rust helper unit tests (cargo)
```

`tests/integration/test_helper_security.py` runs against the built helper;
privileged/GUI integration steps are in `tests/integration/MANUAL.md`.
`tests/regression.sh` runs the full gate (all three Rust crates + the Python suite +
lint + a headless compositor smoke).
