# Veracage

Opens and decrypts VeraCrypt and LUKS volumes and isolates filesystem and clipboard from the rest of the Linux host.
Design in [`docs/veracage-design.md`](docs/veracage-design.md)
Security model in [`docs/SECURITY.md`](docs/SECURITY.md)
Isolation core in [`docs/uid-isolation.md`](docs/uid-isolation.md).

## The problem

Encryption at rest protects a volume only until it is mounted. Once decrypted,
its contents are readable by everything running in your session. Beyond outright
malware, harmless background processes such as search indexers, antivirus
scanners, backup tools, cloud-sync clients, thumbnail generators, and
applications creating autosave or temporary copies can read that plaintext and
leave copies of sensitive data in unencrypted storage outside the volume.

## Architecture

A small root helper (via `pkexec`) opens the volume with `cryptsetup` and
**idmap-mounts** it so its contents are owned by a dedicated `veracage` system
uid, inside a **private mount namespace**. Every non-root uid is denied by
ownership, and the mount never appears in `/proc/mounts`. Apps run as
`veracage` under **bubblewrap** (no network, no host filesystem, curated
`/etc`, `--clearenv`), rendered by a persistent nested Wayland compositor
(`veracage-compositor`) that owns the clipboard and hosts the menu bar.

## Features

- **Single window** - the compositor window, with a File / Clipboard / Apps /
  Settings / Help menu bar. The human-side broker (`veracage-agent`) is
  windowless: it does the host-side work a `veracage`-uid process can't
  (`pkexec`, host file dialogs, `~/.config`) and shows only transient dialogs
  (volume picker, passphrase prompt). Design: `docs/single-window-ux.md`.
- **CLI** - `veracage open <volume> [app]`, `list`, `close`, `close-volume`.
  Opening a volume needs one password prompt (polkit `auth_self_keep`). Apps
  launch from the compositor's Apps menu.
- **Apps menu** - lists the enabled apps with their host icons. The front door
  is an empty session, so apps are launchable before any volume is open:
  a sandboxed **scratchpad** (no network, no host filesystem) whose only ways
  out are the clipboard and the shared directory. Opening a volume joins the
  same session, and apps launched after it see it. Double-clicking a file
  in a sandboxed file manager opens it with your enabled apps: the file-type
  defaults come from what those apps declare, plus your host associations for
  the remaining types. Only enabled apps are ever named, so nothing else can
  be launched inside Veracage.
- **Clipboard** - user-triggered only: Clipboard > Paste in (host to sandbox)
  and Clipboard > Copy out (sandbox to host), with configurable shortcuts
  (defaults Ctrl+Alt+V / Ctrl+Alt+C). Text-only, owned by the compositor. After
  a Copy out, the host clipboard is cleared automatically after a delay
  (default 30 seconds) and again when Veracage quits, so a copied secret does
  not linger on the host. Settings sets the delay, or turns it off.
- **Keyboard** - the compositor builds its keymap from the host desktop's own
  XKB configuration (layout, model, variant, options), so a Compose key or a
  Ctrl/Win mapping set on the host applies inside Veracage too. Settings >
  Keyboard and Shortcuts overrides the Ctrl / Alt / Win part of it, and a
  change applies to the running session.
- **Theme and font** - Settings > Appearance applies to the Veracage window
  live, and to the apps launched next: the session seeds the sandbox's
  `kdeglobals` with the chosen font (in points) and the desktop's own Breeze
  colour scheme, so a dark Veracage runs dark apps. A running app keeps the look
  it started with.
- **Shared directory** - `~/Veracage/Exchange` on the host is idmap-mounted
  into the sandbox at `/exchange`. Files dropped on either side appear on the
  other, owned by the user. No dialogs, no copies. File > Shared
  directory opens it on the host. Disable with `exchange = false`.
- **Multi-volume workspace** - open several volumes and they share one private
  mount namespace, side by side at `/vaults/<label>`. One app set sees them
  all, so a single file manager can drag-and-drop between volumes. The title
  bar counts them ("2 volumes open (work, private)"). **File > Close volume**
  closes one volume (or **All** of them), the rest keep running. Apps see the
  volumes that were open when they launched, which is also why a close refuses
  while an app that saw the volume is still running: it really closes it,
  key out of RAM, so it offers to close that app rather than leave it decrypted.
  Design: `docs/shared-workspace.md`.
- **Quit closes the volumes first** - Veracage never disappears while a volume is
  still decrypted, and never closes an app over its head. Quit (or the window's
  close button) asks every app window to close, so an app with unsaved work
  raises its own prompt and you answer it in the still-open Veracage window.
  Only once they have all closed are the sessions torn down and the volumes
  closed, and only once the last one is really closed does the window go.
- **Any installed app** - `veracage configure` (GUI picker) or
  `--add <binary>` / `--remove <key>` / `--list`. No fixed catalog.
- **Idle close** - Settings > Auto-close after (off, 30 minutes, 1, 2 or
  12 hours). The compositor is the only component that sees whether you are
  using Veracage, so it runs the timer: after that long with no input to the
  Veracage window, the open volumes close themselves and the session stays
  up as an empty scratchpad.
- **Crash-safe teardown** - the session is a `systemd --user` transient
  service whose `ExecStopPost` closes the dm device on any exit
  (SIGKILL, OOM, panic, logout).
- **Suspend** - a root `system-sleep` hook closes every session before
  sleep.
- **Hardened privilege helper** (Rust, `helper-rs/`) - caller uid from
  `PKEXEC_UID` (never argv), continuation pinned at build time, forwarded env
  allowlisted. See `docs/SECURITY.md`.

Deferred: GlobalShortcuts-portal integration for the clipboard keybinds.
`wp_security_context_v1` was evaluated and not adopted
([`docs/mode-a-security-context.md`](docs/mode-a-security-context.md)).

## Install

Debian 12 system packages:

```
sudo apt install bubblewrap cryptsetup python3
```

`bubblewrap` + `cryptsetup` are required. cryptsetup opens both LUKS and
VeraCrypt volumes. The agent and compositor are self-contained Rust binaries
linking only what a desktop session already has (Mesa GL, Wayland/X11,
`libxkbcommon.so.0`).

### Toolchain (build only)

- The privilege **helper** builds on Debian 12's stock `rustc` 1.63.
- The **agent** and **compositor** need rustup + recent stable
  (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`).
  `make install` uses rustup's cargo for both. If `libxkbcommon-dev` is
  absent, `make` synthesises a private linker symlink to `libxkbcommon.so.0`.

```
make install-dev     # build the helper + point a polkit policy at this checkout
make install         # system install to /usr/local (override with PREFIX=)
```

Run `make install` as yourself, NOT under `sudo`: it builds as you and sudoes
only the steps that write outside your home. Under `sudo` the whole build runs
as root, which makes cargo re-fetch the crate index into root's home.

`make install` lays down the package under `PREFIX/lib/veracage`, with
`veracage`, `veracage-agent`, and `veracage-compositor` in `PREFIX/bin` and
the privileged helpers in `PREFIX/libexec/veracage`. It also installs a
polkit policy, a `.desktop` + icon, a udev rule (keeps the dm device out of
UDisks and out of `/dev/disk`), and the `system-sleep` hook, and creates the
`veracage` system user.

### Debian package

```
make deb          # dist/veracage_<version>_<arch>.deb
make install-deb  # build it and hand it to apt (asks before it acts)
```

`make deb` stages an ordinary `make install` into `dist/stage` with
`DESTDIR` set, which skips every step that touches the running system, and
wraps it with `dpkg-deb`. The package installs under `/usr`, so the helper is
rebuilt with `/usr/bin/veracage` as its baked continuation. What `make
install` does to the machine itself, `DEBIAN/postinst` does instead: create
the `veracage` user, add it to `render`, reload the udev rules. The templates
are in `packaging/deb/`.

The version carries the build timestamp
(`0.6.0+<utc>.g<commit>[.dirty]-1`), so a rebuild is always newer than what is
installed and `make install-deb` simply replaces it. Pass `DEB_VERSION=0.6.0-1`
to stamp a release version instead.

`make install-deb` ends by running `./post-install.sh`, if the checkout has
one, with the package it just installed as `$1`. That is an optional hook for
whatever a particular machine wants done with the package, a copy onto a volume
that is only sometimes open for instance, and it is not tracked, so what it does
stays out of the repository.

The polkit policy, the udev rule and the `system-sleep` hook live at fixed
system paths, not under `PREFIX`. A `make install` to `/usr/local` and the
package therefore write the same three files, and whichever ran last owns
them. Pick one, or run `make uninstall` before installing the package. Both
installs then run, but only one is authorised: polkit keys its action to the
pkexec'd program path, so the other matches no action, falls back to
`org.freedesktop.policykit.exec`, and asks for a password at every privileged
step - the teardown that should be silent included. `veracage` says so on
stderr when it sees the mismatch, and `make uninstall` removes those three
shared files only when they are this install's own.

## Usage

```
veracage configure                    # enable apps (GUI picker)
veracage configure --add dolphin      # or by binary name
veracage open /path/to/volume.vc      # open into the shared workspace
veracage open /path/to/volume.vc kate # auto-launch an app too
veracage open /path/to/other.vc       # a 2nd volume joins the same workspace
veracage list /path/to/volume.vc
veracage close-volume <label>         # close one volume (File > Close volume)
veracage close /path/to/volume.vc     # tear the whole session down
```

`pkexec` prompts for the account password, then the CLI prompts for the
volume passphrase on the terminal. The GUI launcher (`veracage-agent`, or the
"Veracage" app-menu entry) collects the passphrase in a window and asks again
if it was wrong. Help > Help in the menu bar has the short usage notes.

## Config

`~/.config/veracage/config.toml` (auto-created):

```toml
[default]
last_used_app  = "kate"
theme          = "light"        # light | dark
ui_font        = "system"       # system (host font) | noto | liberation | dejavu | dejavu-mono
ui_font_size   = "system"       # system (host size) | a point size, e.g. 12
window_size    = "default"      # default | max | 1280x800 (WxH), at start
modifier_keys  = "system"       # system (host mapping) | none | an XKB option,
                                # e.g. altwin:ctrl_win | ctrl:nocaps
suspend_action = "dismount"     # close on suspend, or "ignore" to leave open
clip_clear     = true           # auto-clear the host clipboard after Copy out
clip_clear_timeout = 30         # seconds before the auto-clear fires
auto_dismount  = 0              # idle minutes before the volumes close (0 = off)
debug          = false          # verbose timing logs (docs/debugging.md)

[apps.kate]                     # the enabled-app allowlist (any installed binary)
name     = "Kate"
exec     = "kate"

[volumes."/path/to/work.vc"]    # optional per-volume overrides
default_app  = "okular"
```

Per-volume settings inherit from `[default]`. Only apps under `[apps.*]`
launch. Add one with `veracage configure --add <binary>`.

## Tests

```
make test        # Python unit suite (pytest)
make lint        # ruff + mypy
make test-rs     # Rust helper unit tests (cargo)
make audit       # cargo-audit over all three lockfiles
make deps        # how many crates each binary really pulls in
```

`tests/integration/test_helper_security.py` runs against the built helper.
Privileged/GUI integration steps are in `tests/integration/MANUAL.md`.
`tests/regression.sh` runs the full gate, eleven components: the three Rust
crates (the helper against the distro rustc that holds its MSRV), the Python
suite, lint, the helper argument contract, clippy on all three crates,
cargo-audit, a dependency budget for the privileged helper, a headless
compositor smoke, and the `.deb` build (which also checks that the paths baked
into the packaged helper and polkit policy are the packaged ones).
`tests/audit-reviewed.txt` records the advisories that only an upstream release
can fix, with the reason each was accepted, so a new one fails and a known one
does not.

`python3 tests/mutation_check.py` breaks one security or teardown guard at a
time in a copy of the tree and checks the suite notices. Slower than the gate
(it re-runs the suite per mutation), so run it after touching the sandbox flags,
the teardown paths, or the config validation, not on every commit.

## License

[CC0 1.0 Universal](LICENSE) (public domain).
