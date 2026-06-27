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
  hidden behind a tmpfs (only the Weston socket is bind-mounted in).
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

Known gaps (later):
- Global hotkeys for clipboard transfer are tray-menu only for now (XDG
  GlobalShortcuts portal integration deferred).
- Mode A (`wp-security-context-v1`) deferred — needs Plasma 6 / sway
  1.10+ for testing.

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

Then in the checkout:

```
make install-dev
```

This installs the polkit policy at `/usr/share/polkit-1/actions/org.veracage.policy`
pointing at the dev-path helper.

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

## Tests

```
sudo apt install python3-pytest         # for the unit suite
make test                               # run unit tests
```

Manual / privileged integration tests are documented in
`tests/integration/MANUAL.md` — they need a real vault and Wayland session.

## Adding a new app to the catalog

Edit `src/veracage/apps.py`, add an `App(...)` entry. The `exec` field is
the binary name we look up via `shutil.which`. Re-run `veracage configure`
to detect and enable it.
