# Single-window UX — one window, a menu bar, files in/out

**Status: design of record (2026-07-09). Implementation in progress.** Replaces the
two-floating-window model (standalone launcher window + compositor window) with a
single window whose controls live in a menu bar, plus a mechanism for host↔vault
file transfer. Builds on `uid-isolation.md` (the deny-by-uid architecture).

## The two problems

1. **Two floating windows.** Today the human-uid **launcher** (`veracage-agent`,
   eframe) and the veracage-uid **compositor** are separate top-level windows. The
   launcher lingers next to the compositor after a mount — clutter.
2. **No file transfer.** The old `/vault/.veracage/{in,out}` bridge over the control
   socket was removed (a pen test drove it to exfiltrate the whole vault). We still
   need import/export.

## The constraint that shapes everything

The two windows are separate **processes** *and* separate **uids** (human vs
`veracage`), by necessity: only a human-uid process can `pkexec` / open host files
/ write `~/.config`, and only a veracage-uid process can hold the sealed vault
content. Wayland has no cross-client surface embedding, and a process has one uid,
so **the two cannot be merged into one window, docked, or shown as two frames.**
The only path to one window: the process that *must* have a window — the
compositor — hosts the UI, and delegates the host-side actions to the other.

## The model

- **The compositor window is the only persistent window.** It gains a **menu bar**
  (egui, drawn by the compositor) in place of the current button strip.
- **The human side becomes a windowless broker.** No standing window; it pops only
  transient dialogs (volume picker, passphrase prompt) and runs `pkexec` /
  `veracage open`, driven by commands from the compositor.
- **The front door is the empty compositor window.** Launching Veracage brings up
  the compositor with its menu bar and no vault; **File → Open vault…** then runs
  the volume picker → passphrase → `veracage open`. (Interim in the current build:
  the broker pops the picker at launch instead; the empty-window front door — and
  removing that startup picker — lands in the UI pass, see below.) A `.vc`
  "Open with" opens straight to the passphrase, skipping the picker.

### Menu layout

| Menu | Items |
|---|---|
| **File** | Open vault… · Import file… · Export file… · Close vault · Quit |
| **Edit** | Paste → sandbox (Ctrl+Alt+V) · Copy → host (Ctrl+Alt+C) |
| **Apps** | one item per enabled app of each open vault (launch) |
| **Settings** | Configure apps… · GPU passthrough ☑ · On suspend: Dismount/Ignore |
| **Help** | Keybinds · About |

- **Edit** and **Apps** act **in-process** in the compositor (clipboard is
  compositor-owned; app launch goes to the leader over the veracage-owned socket —
  both already exist).
- **File** and **Settings** items that need host-side work emit a **command** to the
  broker (below).

## Compositor → broker command channel

Generalizes the existing `configure.req` mtime signal into a small verb channel.

- On a menu selection needing the broker, the compositor writes
  `/run/veracage/rt/cmd.req` — one line: `<counter> <verb> [payload]` (verbs:
  `open`, `configure`, `settings`, `close-all`, `import`, `export`). Mode `0644` so the
  human broker can read the verb (`/run/veracage/rt` is `0711 veracage`; the broker
  traverses + reads by exact path). The counter makes each write a distinct event.
- The broker polls the file's mtime (as it already does for `configure.req`) and,
  on a new counter, reads and dispatches the verb.

**Why this is safe.** The authority lives in the **compositor**: a click on its
menu is a real user action a same-uid attacker (Mallory) cannot forge — Mallory is
a different uid, can't be a client of the compositor, and KWin won't grant him
input-synthesis. Mallory **cannot write `cmd.req`** either: `/run/veracage/rt` is
`0711 veracage`, so he can't create files there. The verbs themselves aren't secret
(0644-readable is fine — at most Mallory learns "an import was requested"). So the
broker only ever acts on genuine compositor-issued commands.

## Prompt flow

Cold open shows **two** dialogs, in VeraCrypt order:

1. **Volume passphrase** — a **transient egui dialog** the broker pops, reusing the
   existing hardened field (`Zeroizing<String>`, wiped on every exit path, egui
   undo-history reset). *`systemd-ask-password` was evaluated and ruled out:* its
   agent mode writes a query into `/run/systemd/ask-password/`, which is root-owned
   and not user-writable, so a human-uid process gets `Permission denied` — it is a
   root/system prompt only. Our own dialog is also the more secure choice (we
   control zeroization end-to-end rather than routing the secret through
   `kdialog`/`zenity`).
2. **polkit auth** — the desktop's polkit agent asks for the user's **login
   password** to authorize the root mount helper (`auth_self_keep`, cached ~5 min;
   the compositor bring-up and the mount are **two** pkexecs that `auth_self_keep`
   coalesces into a **single** prompt). Not `sudo`, not root's password.

Subsequent opens while a session is up: **only** the passphrase dialog (polkit
cached), and the new vault's apps join the existing compositor window (no new
window).

## File transfer — the idmapped Exchange folder (DONE, replaces the io-helper)

**Decision (2026-07-09): a shared folder, not dialogs.** Dialog-driven import/export
was rejected as bad UX. Instead, host↔vault transfer is a **reverse-idmapped shared
directory** — the same mechanism we use to map the vault and to map `/usr` for apps,
just pointed at a plain host dir. (Proven: `prototype/spike10` + `spike11`, VPS.)

- **Host side:** `~/Veracage/Exchange` — a plain folder you own, created by
  `cli.py` on open. Nothing is mounted host-side; it's just a directory.
- **Vault side:** the helper idmap-mounts it (`idmap_mount(exchange, …,
  human→veracage)` + `nosuid,nodev,noexec`) into the private NS; the sandbox binds
  it at **`/exchange`** (top-level, NOT under `/vault`, so the "everything in HOME
  is encrypted" invariant holds), with a seeded "Exchange (host-shared)" Place.
- **UX:** drop a file in `~/Veracage/Exchange` on the host → it's instantly in
  `/exchange` in the sandbox, owned by whoever looks (the idmap reverse-maps the
  sandbox's writes back to `1000:1000` on disk, so you own them natively). No
  dialogs, no copies, no daemon. The File menu's Import/Export become "open the
  exchange folder" conveniences, and the whole token-gated `veracage-io` plan is
  **deleted** — this is net-negative code.

This is exactly how VirtualBox shared folders / Podman `--volume :idmap` /
systemd-nspawn `:rootidmap` work; we already shipped the syscall sequence in
`idmap.rs`.

**Security.** The vault is never exposed: `/exchange` is a *separate* mount (a
different superblock from the vault), so `link()` across is `EXDEV`, symlinks
dangle across the boundary, and no vault mount becomes host-visible — the deny-by-
uid seal, the hidden NS, and (critically) the suspend/crash dm-teardown path are all
untouched. What's exposed is exactly what's *in* the exchange — declassified,
in-transit files the user consciously moved there (a same-uid host attacker reading
them is not a vault breach). A *compromised sandboxed app* could copy the vault into
the exchange, but **that is outside the primary threat model** (design §10) — we do
not defend the vault against the apps the user chose to run, so no per-app policy is
needed. Two cheap in-scope guards remain: the helper validates the caller-supplied
exchange path (`O_NOFOLLOW`, owner == human — stops `--exchange /etc`), and the
mount is `nosuid,nodev,noexec`. Per-vault/global `exchange = false` turns it off.
It's independent of any vault (session-level, like the clipboard) and RAM-backing is
a future config knob for no-plaintext-at-rest.

## What changes in code

- **Deleted:** the launcher's persistent menu/settings window (`ui_launcher.rs`
  view machinery). The `configure.req`-specific signal is generalized into `cmd.req`.
- **Compositor:** `toolbar.rs` becomes a menu bar; `winit.rs` dispatches menu
  actions (in-process for clipboard/launch, `request_command` for the rest);
  `request_configure` → `request_command(verb)`.
- **Agent:** the no-args path becomes the headless broker (single-instance guard +
  initial open flow + `cmd.req` watch loop); the hardened passphrase field is
  extracted into a standalone transient window; Settings becomes a transient window.
- **helper-rs:** `--import`/`--export` mode + a polkit action; the compositor issues
  the io token.

## Phases (each gated on `tests/regression.sh`)

1. **DONE (2026-07-09).** Compositor menu bar + generalized `cmd.req` channel — the
   visible "File/Edit menu" change; `request_configure` → `request_command`;
   dropdown input-capture via `wants_pointer`.
2. **DONE (2026-07-09).** Agent → windowless broker + transient one-shot dialogs
   (`_passphrase`, `_settings`, `configure` as fresh processes since winit can't
   reopen an EventLoop); `ui_launcher.rs` deleted; single-instance pid guard. Front
   door is *interim* the broker's `rfd` picker at launch (the empty-compositor front
   door is the target — see Deferred).
3. **DONE (2026-07-09).** File transfer = the idmapped **Exchange folder** (above),
   which *replaced* the token-gated `veracage-io` plan entirely (net deletion).
   Proven by `spike10` (idmap on a plain host dir, both-way rw + write-back owner)
   and `spike11` (end-to-end through the real helper, incl. `--exchange /etc`
   refused). Wired through helper (`--exchange`, `idmap.rs` gained an `extra_attr`
   param), `cli.py`/`config.py` (`exchange = true`), `leader.py`/`sandbox.py`
   (`/exchange` bind + Place). Menu Import/Export can become "open exchange" helpers.

Visual/interactive verification (menu feel, dropdown input capture over the sandbox,
the transient dialogs, the open flow) needs the user's KDE box; the build + the
headless compositor smoke are VPS-tested.
