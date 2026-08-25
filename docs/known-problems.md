# Known problems

Open findings, stored deliberately. The architecture is still moving, so many
items carry an **`obsolete-if:`** tag: if that redesign lands, drop the item
instead of patching doomed code.

Status tags:
- **[FIX]** - real, in code that survives the current design. Fix on its own.
- **[TRACK]** - real but low-priority / cleanup / needs a live desktop.
- **`obsolete-if:`** - a redesign that would delete the finding outright.

---

## A#2 - [TRACK] Helper forwards the trailing continuation argv verbatim
`helper-rs/src/main.rs` execs the pinned `veracage` binary with `args.rest`
(everything after `--`) as argv, **after** dropping to the veracage uid.
**Scope narrowed:** `check_continuation_argv` already pins the *subcommand* to
`{_leader, _compositor}`, so "arbitrary `veracage` subcommand as the veracage
uid" is **blocked**. The residual is only that the trailing *args* to those
two subcommands are forwarded unvalidated (the leader's `--mountpoint` is
injected by the helper itself, `--apps`/`--first` are JSON the leader only
launches from by bounds-checked index). Low residual blast radius.
*Fix:* have the helper **construct** the full continuation argv from validated
inputs rather than forwarding `args.rest`.
`obsolete-if:` the human-uid launcher becomes the only caller AND the helper's
CLI contract is locked to a fixed subcommand set.

## M2 - [TRACK/document] Config-tamper confused-deputy
`config.toml` (human-owned, no integrity check) -> `leader.py` runs its
`exec`/`args` **verbatim** (type-checked, not content-checked) ->
`sandbox.py`. A same-uid attacker rewrites an enabled app to
`exec="/bin/sh", args=["-c","rm -rf /vaults/*"]`. When the human next opens a
volume (typing the passphrase) and clicks it, it runs as the veracage uid with
rw access to the open volumes: volume destruction, or a clipboard-pull staging
point for exfil. The end state overlaps the "malicious sandboxed app"
non-goal, but the *new* thing is redirecting the human's own decryption to
attacker code. Largely inherent to a human-owned config. *Fix:* document that
config integrity is load-bearing (done: `SECURITY.md`). Optionally confirm on
a config-hash change, or move the authoritative app list into veracage-owned
`/run/veracage/rt`.
`obsolete-if:` the enabled-app list moves under `veracage` ownership.

## B#4 - [TRACK] Stale `/run/veracage/rt/app-*` on SIGKILL
`_unpublish_apps` only runs on graceful exit. `cleanup.py` doesn't know about
the `rt/app-*` files. On crash they leak forever (fresh `token_hex(8)` id each
run), leaving dead toolbar buttons (a click hits a dead socket -> harmless
no-op). *Fix:* have the compositor prune a `.apps` whose socket refuses
connection, or have cleanup remove `rt/app-<mount-token>.*`.
`obsolete-if:` the leader<->compositor app channel is replaced (e.g. a live
registration socket instead of files).

## A#4b - [TRACK] No lock around the compositor spawn
Two concurrent first-`open`s both see "compositor down"
(`ensure_compositor_up`) and each `systemd-run`s its own
`veracage-compositor-<hex>` unit. Both run `spawn_compositor`, and the
second's `remove_file(wl-vc)` unlinks the first's live socket. Availability
only (the `0711` dir blocks external planting). *Fix:* an flock around the
check->spawn, or bind without pre-unlinking a live socket.
`obsolete-if:` the compositor bring-up is serialized (a flock, or a single
unit that can't double-start).

## D#7 - [TRACK, needs a live desktop] Flipped180 vs egui orientation
The output renders `Transform::Flipped180` but the toolbar panel + all gating
math assume the strip is at logical-top. If the flip isn't compensated, the
*visible* toolbar and the *input-gated/reserved* strip diverge. In practice
the strip renders at the top and the buttons work, so it currently lines up,
but it's unverified in code and fragile. *Fix:* verify/normalize the paint vs.
hit-test orientation on screen.

## [FIX] The volume picker records the volume path in the host's recent files
`broker.rs` opens the volume through `rfd` built on the **xdg-desktop-portal**
FileChooser, and the portal backend adds every pick to the desktop's recent
documents: measured on KDE, one mount wrote the volume path into both
`~/.local/share/recently-used.xbel` and
`~/.local/share/RecentDocuments/<volume>.desktop`
(`X-KDE-LastOpenedWith=org.freedesktop.impl.portal.desktop.kde`). Recent-files
is a leak class `SECURITY.md` claims to defend, and the entry outlives the
session. Only the path leaks, never volume contents, and the file it names is
still encrypted. The FileChooser portal has no "do not record" option. *Fix:*
pick the volume in our own egui dialog (the agent already has the dialog
infrastructure), which also drops the `rfd` dependency.

## D#8 - [TRACK] Move-grab clamps only the top edge
`move_grab.rs` clamps `y ≥ TOOLBAR_HEIGHT` but nothing else, so a window can
be dragged fully off left/right/bottom and become unreachable (no
overview/keyboard move). Not a crash. *Fix:* keep a visible sliver on every
edge.

## [TRACK, needs a live desktop] Opening a file from the file manager does not raise or focus it
Double-clicking a file in a sandboxed Dolphin opens the app, but its window does
not take focus, and often appears behind Dolphin. Two mechanisms, one symptom:

- **A new window is raised but never focused.** `new_toplevel`
  (`handlers/xdg_shell.rs`) maps with `map_element(..., activate = true)`, which
  smithay documents as "move it to top of the stack" and marks the new window
  active - but the keyboard focus transfer right below it is deliberately
  gated on `current_focus().is_none()` (anti focus-steal). So the new window is
  on top and drawn active while typing still goes to Dolphin.
- **An already-running app gets no new window at all.** KDE apps are
  single-instance over the per-sandbox D-Bus (`dbus-run-session` per bwrap, and
  a file manager spawns the opener *inside its own sandbox*), so the second open
  hands the document to the running instance, which then tries to raise itself.

The Wayland answer to the second one is `xdg_activation_v1`, and the compositor
does not implement it - **but implementing it alone would not fix this host**:
Debian 12's `qtwayland5` / `libqt5waylandclient5` 5.15.8 contain no
`xdg_activation` at all (checked across every `.so` in both packages), so a Qt 5
KDE app cannot request activation from any compositor. Upstream Qt gained it in
6.3.

*Fix:* compositor policy is the only lever that works for both cases today -
focus a newly mapped toplevel unconditionally (optionally behind a setting).
That reverses the anti-focus-steal rule, which is defensible here (a malicious
sandboxed app is outside the threat model and every app is human-enabled) but is
a deliberate decision, not a bug fix. Add `xdg_activation_v1` as well when the
app set moves to Qt 6, so a well-behaved client can ask instead of the
compositor guessing.
`obsolete-if:` the sandbox becomes one bwrap instance per app *launch* (no
shared D-Bus, so every open maps a fresh toplevel) AND focus-on-map lands.

## [TRACK] Live window resize was removed, not fixed
Settings > Appearance used to resize the running window. It was withdrawn (the
size now applies when the window is created, and the dropdown says so) because
the resize left the menu bar laid out for the OLD width: the strip drawn at one
size and hit-tested at another, flicker, and clicks landing on the wrong menu.

**Dragging the window edge is fine** and always was, which is the clue: a manual
resize arrives as a host configure -> `WindowEvent::SurfaceResized` ->
`WinitEvent::Resized`, and that arm updates the output mode, forces a full
redraw and re-lays out the strip. The programmatic path produced no such event.
winit documents `request_surface_size` as "the applied size will be returned
immediately, resize event in such case may not be generated", and it returned
`Some(1280x800)` - applied, no event. So the surface resized while the output
mode, egui's screen rect and the pointer gating all stayed at the old size.

*Fix, if the setting is ever wanted live again:* when `request_surface_size`
returns `Some(size)`, run the same update the `Resized` arm does rather than
waiting for an event that is not coming. Roughly five lines, but it needs a live
desktop to confirm, and the setting reads fine as a start-time one.

## Misc LOW - residuals + hardening
- **`.apps` world-readable**: `leader.py` `write_text` creates
  `/run/veracage/rt/<id>.apps` 0644 in the 0711 dir -> any uid reads the
  volume label + enabled-app names during a session. *Fixed:* `_write_apps_file`
  now writes a pid-tagged temp file, chmods it 0600 and renames it into place
  (which also removes the torn-read window the compositor's poll could hit).
- **Volume mounts not `noexec`**: `helper-rs/main.rs` mounts `nodev,nosuid`
  only. Add `noexec` to block direct `execve` of a volume-resident binary
  (interpreted files still run). Gate per-volume if "run a binary from the
  volume" is ever wanted.
- **bwrap `--unshare-user --disable-userns`** (bwrap >= 0.8) would narrow the
  kernel attack surface a compromised app can reach. Confining the app is
  outside the threat model, so this is defense in depth only, not a fix this
  model owes. (The core-dump leak that used to be filed here is fixed: the
  helper clears `coredump_filter` for everything it execs. `RLIMIT_CORE=0`,
  which this item used to prescribe, does nothing on a systemd host.)
- **`scan_leaders` TOCTOU** - residual on the existing size/type checks:
  between `symlink_metadata` and `read_to_string` a same-uid process can swap
  the validated regular file for a FIFO (main-thread hang) or grow it past the
  64 KB cap. Same trust domain (a compromised leader). The sandbox app has no
  `rt/` in its mount ns. *Fix:* open `O_NONBLOCK|O_NOFOLLOW`, `fstat` the
  opened fd, bounded read. Also cap `.apps` file count + names-per-file.
- **resize negative `max_size`**: `resize_grab.rs` floors `min_size` at 1 but
  not `max_size`. A client's negative `set_max_size` yields a below-min
  (possibly negative) configure to that same client. Self-inflicted. *Fix:*
  clamp `max ≥ min`.

## Misc LOW (record, revisit opportunistically)
- **A#8** `--passphrase-stdin` with an interactive tty (no EOF) blocks the
  root helper forever (self-inflicted, add an isatty/timeout guard).
- **B#5** leader serve loop is single-threaded (slowloris: a peer that never
  sends stalls control + launches for 2 s, same-uid only).
- **B#6** compositor liveness latched once in the leader. If it dies later,
  launches silently fail (bwrap binds a missing socket).
- **B#8** `bwrap`/`app.exec` invoked by name via `$PATH` (depends on the
  helper's env hygiene, prefer absolute `/usr/bin/bwrap`).
- **compositor client-reachable `unwrap`s: verified safe.** Every
  client-triggerable `unwrap`/`expect` was cross-checked against the pinned
  smithay source and confirmed safe today: `send_configure().expect` (guarded
  by `!is_initial_configure_sent`), DnD `grab_start_data().unwrap()` (guarded
  by `has_grab`), `from_bits(edges).unwrap()` (only `WEnum::Value` edges reach
  it), and the `insert_client`/`dispatch_clients` ones rest on
  compositor-owned invariants. Standing caveat: `input.rs`/`compositor.rs`
  `toplevel().unwrap()` is safe only while every mapped window is an xdg
  toplevel. Adding XWayland or layer-shell must switch these to the guarded
  `window_for_surface` form.
- **build hygiene**: the `libxkbcommon.so` linker-symlink synthesis lives in
  the `Makefile`. Moving it to `compositor-rs/build.rs` would let
  rust-analyzer / plain `cargo` / CI inherit it.
