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
two subcommands are forwarded unvalidated (`--mountpoint` is separately
validated, `--apps`/`--first` are JSON the leader only launches from by
bounds-checked index, and `--gpu` is a bool). Low residual blast radius.
*Fix:* have the helper **construct** the full continuation argv from validated
inputs rather than forwarding `args.rest`.
`obsolete-if:` the human-uid launcher becomes the only caller AND the helper's
CLI contract is locked to a fixed subcommand set.

## Passphrase not zeroized in the root helper - [TRACK]
The passphrase lives unwiped in the *root* helper's heap (COW-inherited by the
forked children). Only root can read that, and root already wins, so it's low
priority. *Fix:* `zeroize` the bytes after `crypt::open`. `obsolete-if:` the
mount stops handling a plaintext passphrase (TPM/FIDO2 or keyring unlock).

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

## D#8 - [TRACK] Move-grab clamps only the top edge
`move_grab.rs` clamps `y ≥ TOOLBAR_HEIGHT` but nothing else, so a window can
be dragged fully off left/right/bottom and become unreachable (no
overview/keyboard move). Not a crash. *Fix:* keep a visible sliver on every
edge.

## Misc LOW - residuals + hardening
- **`.apps` world-readable**: `leader.py` `write_text` creates
  `/run/veracage/rt/<id>.apps` 0644 in the 0711 dir -> any uid reads the
  volume label + enabled-app names during a session. *Fix:*
  `os.open(..., 0o600)`.
- **Volume mounts not `noexec`**: `helper-rs/main.rs` mounts `nodev,nosuid`
  only. Add `noexec` to block direct `execve` of a volume-resident binary
  (interpreted files still run). Gate per-volume if "run a binary from the
  volume" is ever wanted.
- **`cleanup.py` `..` in mountpoint**: the post-close scratch delete gates on
  `startswith("/run/veracage/")`, which allows `..`, a latent root
  arbitrary-delete (`rmdir`/`rmtree`), not reachable today (the lock is
  root-owned, the helper writes a validated path). *Fix:* match the helper's
  `mountpoint_ok` (parent == `/run/veracage`, reject `..`), applied to the
  `.raw`/`.run` siblings too.
- **bwrap nested userns + cores**: `sandbox.py` doesn't `--disable-userns`
  (a compromised viewer can `unshare(CLONE_NEWUSER)` to widen kernel attack
  surface) nor suppress cores (a crash dumps decrypted content to
  `/var/lib/systemd/coredump`). *Fix:* `--unshare-user --disable-userns`
  (bwrap >= 0.8) + `RLIMIT_CORE=0` preexec. Secondary threat model.
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
- **Stale `helpers/veracage-helper`**: the legacy Python helper drops to the
  **caller** uid with **no idmap** (bypasses the entire isolation model) and
  is still wired as a `cli.py` fallback when the Rust binary is absent. Can't
  run as root today (polkit pins `exec.path` to the Rust helper) but it's a
  full-model bypass one policy edit away, and shares the `..` mountpoint
  weakness. *Fix:* delete it + the `cli.py` fallback (hard-error "build the
  Rust helper" instead).

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
