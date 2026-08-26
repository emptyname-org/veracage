# Veracage

Veracage isolates opened LUKS and VeraCrypt volumes from the rest of the Host.

The encrypted container can live anywhere on the host, including an ordinary unencrypted filesystem, as long as you could open it yourself: the helper refuses a source you have no read-write access to, so it cannot be used to make root open a container you have no permission for. Once opened, its plaintext is accessible only inside a separate sandbox running as the dedicated `veracage` system user.

Host applications running as your normal uid cannot read the mounted volume. This includes indexers, backup tools, cloud-sync clients, thumbnailers, clipboard managers, antivirus scanners, and applications that like to leave autosaves or temporary files behind.

Root is outside the threat model.

Design: [`docs/veracage-design.md`](docs/veracage-design.md)  
Security model: [`docs/SECURITY.md`](docs/SECURITY.md)  
UID isolation: [`docs/uid-isolation.md`](docs/uid-isolation.md)

## How it works

Veracage uses three separate barriers:

1. **Different uid**

   A small root helper opens the volume with `cryptsetup` and idmap-mounts it as the `veracage` system uid.

   Nothing is rewritten on disk. The mounted filesystem is simply presented with different ownership, so your normal Host uid cannot read it.

2. **Private mount namespace**

   The decrypted filesystem exists only inside Veracage's mount namespace. It is not mounted into the host namespace.

3. **Application sandbox**

   Applications run under `veracage` inside `bubblewrap`, with:

   - no host filesystem
   - no network
   - curated `/etc`
   - cleared environment
   - `HOME=/vaults`

   Their UI is displayed through a persistent nested Wayland compositor, `veracage-compositor`.

The result is a separate workspace for decrypted files rather than another encrypted-volume mounter.

## Privileged component

The root helper has one job: create and tear down the isolated environment.

It opens the volume, creates the idmapped mount and namespace, then starts the session leader as `veracage`. A parent process remains root only so it can close the volume when the session ends.

After setup it accepts no commands and listens on nothing.

There is no setuid binary.

The helper also does not trust command-line input for security-sensitive values:

- the calling uid comes from `PKEXEC_UID`
- the continuation executable is fixed at build time
- forwarded environment variables are allowlisted
- unknown arguments are rejected
- mountpoints are derived from the volume label rather than supplied by the caller

The helper is written in Rust and deliberately has a small dependency tree.

See [`docs/uid-isolation.md`](docs/uid-isolation.md) for the exact setup.

## Workspace

Veracage presents one window containing the sandboxed applications.

The host-side `veracage-agent` is only a broker for operations that the sandbox cannot perform itself, such as:

- `pkexec`
- host file dialogs
- reading/writing Veracage configuration
- collecting the volume passphrase

The agent never receives access to the decrypted filesystem.

You can start Veracage without opening a volume. This gives you an empty sandbox with no network and no host filesystem. Opening a volume later adds it to the existing workspace.

Multiple volumes can be open at once:

```text
/vaults/work
/vaults/archive
/vaults/private
```

Applications launched after a volume is opened can see it. A volume cannot be closed while an application that has access to it is still running.

## Applications

Any installed application can be enabled:

```bash
veracage configure
veracage configure --add dolphin
veracage configure --add kate
```

Only enabled applications can be launched inside Veracage.

File associations are built from those applications and, where needed, the host's existing associations.

There is no fixed application catalog.

## Clipboard

Clipboard transfer is explicit.

From the Veracage menu:

```text
Clipboard > Paste in
Clipboard > Copy out
```

Default shortcuts:

```text
Ctrl+Alt+V    Host -> Veracage
Ctrl+Alt+C    Veracage -> Host
```

Only text is transferred.

The nested compositor owns the internal clipboard and does not expose the Wayland `data-control` protocol, so applications inside the sandbox cannot silently scrape clipboard contents.

After **Copy out**, Veracage can automatically clear the host clipboard. The default delay is 30 seconds. It also attempts to clear copied text when Veracage exits.

Both behaviours are configurable.

## Shared directory

For deliberate file exchange, Veracage exposes:

```text
~/Veracage/Exchange   <->   /exchange
```

The directory is idmap-mounted into the sandbox with:

```text
nosuid,nodev,noexec
```

On the Host the files are owned by your normal uid. Inside the sandbox the idmap presents them as `veracage`, like the volumes.

The exchange directory is a different filesystem from the encrypted volume, so hard links across the boundary fail with `EXDEV`. Symlinks pointing outside the sandbox do not provide a route into the host filesystem.

Disable it with:

```toml
exchange = false
```

## Closing volumes

Closing a volume actually closes it. Veracage does not leave the decrypted mapping around after removing it from the UI.

If an application still has access to the volume, Veracage asks to close that application first.

Quitting Veracage follows the same order:

1. ask application windows to close
2. let applications handle unsaved work normally
3. tear down the sandbox sessions
4. close the encrypted volumes
5. close the Veracage window

### Idle close

Veracage can automatically close open volumes after a period with no input to the Veracage window.

Available settings include:

```text
off
30 minutes
1 hour
2 hours
12 hours
```

The empty sandbox can remain running after the volumes are closed.

### Suspend

By default, a `system-sleep` hook closes Veracage sessions before suspend.

This can be disabled in configuration.

### Crash teardown

The root parent closes the volumes whenever the session leader exits, however it exits.

The session also runs as a transient `systemd --user` service whose `ExecStopPost` repeats that teardown for the case where the helper is gone too. Between them this covers `SIGKILL`, OOM termination, logout, and compositor failure.

## Memory and dumps

Veracage clears `coredump_filter` before starting the session, and the setting is inherited by the compositor, session leader, `bubblewrap`, and sandboxed applications.

The volume passphrase is passed to the helper through stdin from a zeroizing buffer and overwritten after `cryptsetup` consumes it.

The decrypted device is created as:

```text
root:disk 0660
```

with `UDISKS_IGNORE=1`.

Its filesystem label and UUID are also kept out of `/dev/disk`.

See [`docs/SECURITY.md`](docs/SECURITY.md).

## Keyboard and appearance

The compositor starts from the host XKB configuration, read from KDE's `kxkbrc`: layout, model, variant, and options. Where none is detectable, libxkbcommon's own defaults apply.

Compose keys and remapped Ctrl/Alt/Win keys therefore behave the same way inside Veracage unless overridden in:

```text
Settings > Keyboard and Shortcuts
```

Appearance settings control the Veracage window and applications launched afterward.

The sandbox receives a generated `kdeglobals`: the selected font, and a look. Follow the Host, the default, copies the Host's own colour scheme, widget style and icon theme out of its `kdeglobals`, which is where KDE writes whatever scheme is applied, so apps match the rest of the screen whatever that scheme is. Light and Dark force Breeze light or dark instead. An icon theme installed only under your home is dropped, since the sandbox cannot see it, and with no host `kdeglobals` at all only the fonts are set.

## Requirements

From the machine:

- Wayland session
- Linux 5.12+ (idmapped mounts)

From packages, which the Debian package pulls in itself:

- `polkit`
- `systemd`
- Python 3.11+
- `bubblewrap`
- `cryptsetup`

Developed and run on Debian 12 under KDE Plasma. Debian 13 is covered by the build and the headless tests.

A source install needs those packages first:

```bash
sudo apt install bubblewrap cryptsetup python3
```

`cryptsetup` handles both LUKS and VeraCrypt volumes. VeraCrypt containers use cryptsetup's `tcrypt` support, so the VeraCrypt binary itself is not required.

## Build

The privileged helper builds with Debian 12's stock Rust 1.63.

The agent and compositor require a recent stable Rust toolchain through `rustup`.

Development install:

```bash
make install-dev
```

`make install-dev` points polkit at the binaries in the checkout, so anything that can write there gets root without a prompt. Single-user or disposable machines only.

System install:

```bash
make install
```

Run `make install` as your normal user, not through `sudo`. The Makefile elevates only the installation steps that need root.

The default prefix is `/usr/local`; override it with `PREFIX=`. The Debian package installs under `/usr`, and `make deb` only builds it.

A system install adds:

- `veracage`
- `veracage-agent`
- `veracage-compositor`
- privileged helper
- polkit policy
- desktop file and icon
- udev rule
- suspend hook
- `veracage` system user

## Debian package

Build:

```bash
make deb
```

The package is written to:

```text
dist/veracage_<version>_<arch>.deb
```

Package builds install under `/usr`.

For a release build:

```bash
DEB_VERSION=0.7.0-1 make deb
```

`make deb` builds the package, it does not install it. Install it yourself with `sudo apt install ./dist/...deb`, and then do not keep a `make install` beside it unless you know which one owns the shared polkit, udev, and suspend-hook files. Only the install that wrote the polkit policy is authorised, and the other asks for a password at every step.

Use:

```bash
make uninstall
```

before switching installation methods.

## Usage

Start Veracage:

```bash
veracage
```

Configure applications:

```bash
veracage configure
veracage configure --add dolphin
```

Open a volume:

```bash
veracage open /path/to/volume.vc
```

Open it and immediately launch an application:

```bash
veracage open /path/to/volume.vc kate
```

Add another volume to the same workspace:

```bash
veracage open /path/to/other.vc
```

List:

```bash
veracage list /path/to/volume.vc
```

Close one volume:

```bash
veracage close-volume <label>
```

Close the whole session:

```bash
veracage close /path/to/volume.vc
```

`pkexec` uses the host's normal polkit agent. The CLI reads the volume passphrase from the terminal. The graphical launcher asks for it in a dialog and prompts again after a wrong passphrase.

## Configuration

Configuration lives in:

```text
~/.config/veracage/config.toml
```

Example:

```toml
[default]
last_used_app      = "kate"

theme              = "system"      # system (follow the Host) | light | dark
ui_font            = "system"
ui_font_size       = "system"
window_size        = "default"

modifier_keys      = "system"

suspend_action     = "dismount"    # dismount | ignore

clip_clear         = true
clip_clear_timeout = 30

auto_dismount      = 0             # idle minutes, 0 = disabled
debug              = false         # verbose logs, see docs/debugging.md
log_dir            = "/home/you/logs"      # optional, must be writable by uid veracage

[apps.kate]
name = "Kate"
exec = "kate"

[volumes."/path/to/work.vc"]
default_app = "okular"
```

Per-volume settings inherit from `[default]`.

Only applications listed under `[apps.*]` can be launched.

## Tests

```bash
make test        # Python tests
make lint        # ruff + mypy
make test-rs     # Rust helper tests
make audit       # cargo-audit
make deps        # dependency counts
```

Integration tests for the privileged helper:

```text
tests/integration/test_helper_security.py
```

Manual privileged/GUI tests:

```text
tests/integration/MANUAL.md
```

Full regression gate:

```bash
tests/regression.sh
```

For changes to sandbox flags, teardown paths, or configuration validation:

```bash
python3 tests/mutation_check.py
```

## License

[CC0 1.0 Universal](LICENSE)