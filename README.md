# Veracage

VeraCrypt sandboxing wrapper. See `veracage-design.md` for the full design.

## Status

**Slice 4.** Slice 3b + crash-safe cleanup via systemd transient scope
+ suspend handling via `login1`.

What works:
- `veracage configure` — pick installed apps via Qt window (or
  `--text` / `--auto` / `--list`).
- `veracage open <vault.vc> [app]` — mount in a private mount NS, spawn
  nested `weston`, run the first app, become session leader, listen on
  a per-vault UNIX control socket.
- `veracage exec <vault.vc> <app>` — add another app to the same session
  (no second password prompt; runs inside the same Weston window).
- `veracage list <vault.vc>` — show running apps.
- `veracage close <vault.vc>` — gracefully tear down the session.
- A **tray icon** appears while a session is open. Menu items:
  - Open app in vault → submenu of enabled apps
  - Push host clipboard → sandbox / Pull sandbox clipboard → host
  - Show drop zone (host file → vault inbox)
  - Export from sandbox (vault outbox → host file)
  - Close vault
- **Drop zone** window: drag any host file onto it → copied into
  `/vault/.veracage/in/` (sandbox apps see it via `Open` dialog).
- **Outbox watcher**: anything saved to `/vault/.veracage/out/` triggers a
  tray notification; click the tray icon to export to a host path.
- bwrap flags: `--unshare-pid/uts/ipc/cgroup/net`, host runtime dir
  hidden behind a tmpfs (only the Weston socket is bind-mounted in),
  curated `/etc` (linker/fontconfig/tz/NSS/machine-id/XDG/TLS only, not
  all of host `/etc`).
- App allowlist driven by `~/.config/veracage/config.toml`.
- On session end: SIGTERM remaining apps → kill agent → kill weston → dismount.
- **Crash-safe cleanup.** The launcher runs inside a `systemd-run --user
  --scope` transient unit. Its `ExecStopPost` invokes
  `pkexec veracage-cleanup --vault-hash <h>`, which reads
  `/run/veracage/<h>.lock` and `cryptsetup close`s the dm-crypt device.
  This fires on SIGKILL, OOM, panic, log-out — anything that destroys
  the scope. The cleanup polkit action allows active sessions without a
  prompt (`<allow_active>yes</allow_active>`); the helper rejects any
  `dm_name` not matching `veracage-[0-9a-f]{12}` for safety.
- **Suspend handling.** The agent subscribes to
  `org.freedesktop.login1.Manager.PrepareForSleep`; on suspend, sends
  `close` to the session leader so the dm-crypt key isn't left in RAM.
  Requires `python3-gi`; soft-fails otherwise.
- **Privilege helper in Rust** (`helper-rs/`), polkit-authorised. Derives
  the caller's uid/gid from `PKEXEC_UID` (never argv), pins the continuation
  at build time, and allowlists forwarded env — closing a local
  privilege-escalation hole. A Python reference helper is kept as the
  pre-build fallback. See `SECURITY.md`.
- **Config**: per-volume settings (`[volumes."<path>"]`), a `gpu` opt-in
  (`/dev/dri` passthrough, default off) and `suspend_action`
  (`dismount` | `ignore`). See [Config](#config) below.
- ruff + mypy clean; GitHub Actions CI (Python + Rust helper); 123 unit tests.

Known gaps (later):
- Global hotkeys for clipboard transfer are tray-menu only for now (XDG
  GlobalShortcuts portal integration deferred).
- Mode A (`wp-security-context-v1`) **intentionally not adopted** — it does
  not isolate the clipboard (Mode B's separate compositor is what closes req
  1.1.2), and GNOME/Mutter doesn't implement it. The full implementation spec
  is kept in `docs/mode-a-security-context.md` if the trade-off ever changes.

## Install (development)

System packages on Debian 12:

```
sudo apt install bubblewrap cryptsetup veracrypt weston wl-clipboard \
                 python3 python3-pyside6.qtwidgets python3-gi
```

- `python3-pyside6.qtwidgets` — config screen + tray UI (agent). Without
  it the launcher/CLI still work; tray/drop-zone/clipboard bridge don't.
- `wl-clipboard` — clipboard bridge.
- `python3-gi` — suspend handling via login1. Soft-fail without it.

The privilege helper is written in Rust, so you also need a toolchain
(`rustup`, or distro `cargo` + `rustc`).

Dev install — builds the helper and points a polkit policy at this checkout:

```
make install-dev
```

System install to `/usr/local` (override with `PREFIX=`):

```
sudo make install
```

`make install` lays down the package under `PREFIX/lib/veracage`, the
launcher at `PREFIX/bin/veracage`, the privileged helpers under
`PREFIX/libexec/veracage`, and a polkit policy generated from
`install/org.veracage.policy.in`. `make build` / `make test-rs` build and
test just the Rust helper.

## First run

```
src/bin/veracage configure         # Qt window; tick the apps you want
# or:
src/bin/veracage configure --auto  # enable everything detected
src/bin/veracage configure --list  # see catalog state
```

Config file: `~/.config/veracage/config.toml` (auto-created).

## Open a vault

```
src/bin/veracage open /path/to/vault.vc          # uses last app, or first enabled
src/bin/veracage open /path/to/vault.vc kate
src/bin/veracage open /path/to/vault.vc okular
```

`pkexec` will prompt for your account password (polkit), then `cryptsetup`
prompts for the vault password on the same terminal.

## Config

`~/.config/veracage/config.toml` (auto-created by `configure`):

```toml
[default]
last_used_app  = "kate"
gpu            = false          # /dev/dri passthrough (side channel; off)
suspend_action = "dismount"     # or "ignore" to keep mounted across suspend

[apps.kate]                     # the enabled-app allowlist
name     = "Kate"
category = "text"
exec     = "kate"
args     = ["/vault"]

[volumes."/home/you/Documents/work.vc"]   # optional per-volume overrides
default_app  = "okular"
gpu          = true             # overrides [default].gpu for this vault
```

Per-volume settings inherit from `[default]`. Only apps under `[apps.*]`
are launchable.

## Tests

```
make test        # Python unit suite (pytest)
make lint        # ruff + mypy (needs .venv dev deps)
make test-rs     # Rust helper unit tests (cargo)
```

`tests/integration/test_helper_security.py` runs against the built Rust
helper (after `make build`). The remaining privileged/GUI integration tests
are documented in `tests/integration/MANUAL.md` — they need a real vault and
Wayland session.

## Adding a new app to the catalog

Edit `src/veracage/apps.py`, add an `App(...)` entry. The `exec` field is
the binary name we look up via `shutil.which`. Re-run `veracage configure`
to detect and enable it.
