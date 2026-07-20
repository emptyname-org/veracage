# Single-window UX

One persistent window, the compositor's, with a menu bar. The human side is
a windowless broker. Builds on `uid-isolation.md`.

## The constraint that shapes it

The compositor and the human-side agent are separate **processes** *and*
separate **uids** (human vs `veracage`), by necessity: only a human-uid
process can `pkexec`, open host files, or write `~/.config`, and only a
veracage-uid process can hold the sealed volume content. Wayland has no
cross-client surface embedding, and a process has one uid, so the two cannot
be merged into one window, docked, or shown as two frames. The one-window
path: the process that *must* have a window, the compositor, hosts the UI
and delegates host-side actions to the broker.

## The model

- **The compositor window is the only persistent window**, with an egui menu
  bar in its chrome.
- **The human side is a windowless broker** (`veracage-agent`): no standing
  window. It pops only transient dialogs (volume picker, passphrase prompt)
  and runs `pkexec` / `veracage open`, driven by commands from the compositor.
- **Front door**: launching Veracage brings up the compositor **and an empty
  session** - a session leader with no volume (`veracage _up` runs
  `ensure_compositor_up` then `ensure_empty_session`, two pkexecs coalesced
  into one prompt). So apps are launchable immediately, sandboxed against an
  empty workspace plus `/exchange` - a secure scratchpad (open an editor, work
  with no network or host filesystem, then clipboard-out or save to the
  shared directory). File > Mount volume adds a volume to that same session via
  the add-volume path. Apps launched after the mount see it (a fixed-at-launch
  mount namespace, same rule as multi-volume). If the empty session can't
  bootstrap, the Apps menu falls back to showing the configured apps disabled
  until a volume is mounted (the compositor reads `pub/config.apps`).

### Menu layout

| Menu | Items |
|---|---|
| **File** | Mount volume... / Shared directory / Unmount > (one item per mounted volume) / Quit |
| **Clipboard** | Copy out / Paste in (configurable shortcuts, defaults Ctrl+Alt+C / Ctrl+Alt+V) |
| **Apps** | one item per app, with its host icon / Configure apps... |
| **Settings** | Settings... (theme, window size, font, clipboard clear, shared directory, suspend, GPU) / Configure shortcuts... |
| **Help** | Help... / About Veracage... |

**Clipboard** and **Apps** (launch, with a volume mounted) act in-process in the
compositor: the clipboard is compositor-owned, and an app launch goes to the
leader over the veracage-owned socket. Everything needing host-side work
(open, exchange, configure, settings, help, about, unmount) emits a command
to the broker.

## Compositor -> broker command channel

On a menu selection needing the broker, the compositor writes
`/run/veracage/rt/cmd.req`, one verb line (verbs: `open`, `configure`,
`settings`, `shortcuts`, `exchange`, `help`, `about`, `close-volume:<label>`.
Apps launch in-process over the leader socket, not through the broker),
mode 0644 (`/run/veracage/rt` is `0711 veracage`, so the broker traverses
and reads by exact path). The broker polls the file's mtime and dispatches
new verbs.

**Why this is safe.** The authority lives in the compositor: a click on its
menu is a real user action a same-uid attacker cannot forge. The attacker is
a different uid than `veracage`, can't be a client of the compositor, and the
host compositor won't grant it input synthesis. It cannot write `cmd.req`
either: `/run/veracage/rt` is `0711 veracage`, so it can't create files
there. The verbs themselves aren't secret (0644-readable is fine, at most an
observer learns "an open was requested"). The broker only ever acts on
genuine compositor-issued commands.

## Publishing the app list (front-door Apps menu)

The compositor runs as the veracage uid and cannot read the human's
`~/.config`, so the human side publishes the configured apps for it: the
helper creates `/run/veracage/pub` (root-created inside root-owned
`/run/veracage`, then handed to the human uid, mode 0755) at compositor
spawn, and the broker/CLI write `config.apps` (`<key>\t<name>` per line)
plus `icons/<key>.rgba` menu icons resolved from the host icon theme. The
broker republishes whenever config.toml changes. Trust level: the dir is
writable only by the human uid, the same trust as config.toml itself. The
compositor validates everything it reads from there (sizes, key shapes,
icon dimensions).

## Prompt flow

Cold open shows two dialogs, in VeraCrypt order:

1. **Volume passphrase**, collected by the broker: `kdialog --password` when
   available (it matches the desktop's polkit prompt look), else a transient
   egui dialog with a hardened field (`Zeroizing<String>`, wiped on every
   exit path, egui undo-history reset). A wrong passphrase does not fail
   silently: the helper exits with a dedicated code (4, "decrypt failed")
   and the broker re-opens the prompt with a "wrong passphrase" line until
   it succeeds or the user cancels.
2. **polkit auth**: the desktop's polkit agent asks for the user's **login
   password** to authorize the root mount helper (`auth_self_keep`, cached
   ~5 min, so the compositor bring-up and the mount are two pkexecs coalesced
   into a **single** prompt). Not `sudo`, not root's password.

Subsequent opens while a session is up: **only** the passphrase dialog
(polkit cached), and the new volume joins the existing compositor window (no
new window).

## File transfer, the shared directory

Host-to-Veracage transfer is a **reverse-idmapped shared directory**, the
same mechanism used to map the volume, pointed at a plain host dir. Not
dialogs, not a daemon.

- **Host side:** `~/Veracage/Exchange`, a plain directory owned by the user,
  created on open. Nothing is mounted host-side. File > Shared
  directory opens it in the host file manager.
- **Veracage side:** the helper idmap-mounts it (human to veracage,
  `nosuid,nodev,noexec`) into the private NS. The sandbox binds it at
  **`/exchange`** (top-level, NOT under `/vaults`, so the "everything in
  HOME is encrypted" invariant holds), with a seeded "Exchange (host-shared)"
  Place.
- **UX:** a file dropped on either side appears instantly on the other,
  owned natively by the user. The idmap reverse-maps the sandbox's writes
  back to the human uid on disk. No dialogs, no copies, no daemon.

This is how VirtualBox shared folders, Podman `--volume :idmap`, and
systemd-nspawn `:rootidmap` work. The syscall sequence is the helper's
existing `idmap.rs`.

**Security.** The volume is never exposed: `/exchange` is a *separate* mount
(a different superblock from the volume), so `link()` across is `EXDEV`,
symlinks dangle across the boundary, and no volume mount becomes
host-visible. The deny-by-uid seal, the hidden NS, and the suspend/crash
dm-teardown path are untouched. What's exposed is exactly what is *in* the
exchange: declassified, in-transit files the user consciously moved there (a
same-uid host attacker reading them is not a volume breach). A *compromised
sandboxed app* could copy volume files into the exchange, but that is
outside the primary threat model (`veracage-design.md` section 10). Veracage
does not defend the volume against the apps the user chose to run. Two cheap
in-scope guards: the helper validates the caller-supplied exchange path
(`O_NOFOLLOW`, owner == human, which stops `--exchange /etc`), and the mount
is `nosuid,nodev,noexec`. Per-volume/global `exchange = false` turns it off.
It is session-level (like the clipboard), independent of any volume.
RAM-backing is a future config knob for no-plaintext-at-rest.
