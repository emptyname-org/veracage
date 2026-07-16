# Shared workspace redesign — N volumes, one sandbox, drag-and-drop

**Status: Phases 1–6 BUILT + VPS-proven (via spike8/12/13/15/16/17).**
Phase 1 (idmap-into-setns) ✅ spike8. Phase 2 (session bootstrap) ✅ spike12.
Phase 3 (setns add-volume + multi-volume teardown) ✅ spike13. Phase 4 (sandbox
binds the `/vaults` tree, one global app set, multi-volume Places) ✅. Phase 5
(compositor per-volume close — the global Apps menu is already one list per the
single session leader) ✅ spike15. Phase 6 (two-volume DnD + docs) ✅; the one
box-gated check remaining is the real two-volume Dolphin drag-and-drop e2e.
Supersedes the per-vault sandbox model for the multi-volume case. Builds on
`uid-isolation.md` (deny-by-uid + the persistent one-compositor architecture).

## Goal

Open several encrypted volumes at once, have them share **one set of apps**, and
support **drag-and-drop of files between volumes** inside a single file-manager
window. Cross-volume isolation is explicitly *not* a goal (all volumes are the
same human's own data — see `uid-isolation.md` §"Not a goal"). Volume mounts stay
**hidden** (not visible in the host `/proc/mounts`).

## Current model (baseline)

Each `veracage open <vault>` is independent: its own `pkexec` helper, its own
`unshare(CLONE_NEWNS)`, its own idmap mount at `/run/veracage/<hash>`, its own
leader, its own dm device + lock. Apps run in a bwrap that binds only *that*
volume at `/vault`. Two volumes ⇒ two private namespaces ⇒ no process ever sees
both ⇒ no shared path ⇒ file copy/DnD between them is impossible (text clipboard
still crosses, via the shared compositor selection).

## Target model

One **workspace**: a single private mount namespace that holds *all* open volumes.

```
workspace mnt NS (private — hidden from host /proc/mounts)
  /vaults/<labelA>/     ← idmap mount of volume A (owner → veracage uid)
  /vaults/<labelB>/     ← idmap mount of volume B
  …
```

- **One session leader** runs in the workspace NS (as the veracage uid) and holds
  it open for the session's lifetime.
- **One app set** (the human's configured apps). Launching an app runs it in a
  bwrap that binds the whole `/vaults` tree, so a single Dolphin shows
  `/vaults/labelA` and `/vaults/labelB` side by side — DnD between them is an
  ordinary in-process move. Text clipboard still crosses as today.
- The **compositor** is unchanged (one persistent instance; apps connect by
  socket). It does not need the mounts, so it stays in its own namespace.

## Lifecycle

### First open (bootstrap the session)
`veracage open <A>` → the CLI first brings up the compositor as its **own**
`systemd --user` unit (`veracage-compositor-<hex>.service`, if not already up, so
it survives this session's teardown), then wraps the mount in a `systemd --user`
transient unit (the **session anchor**; its `ExecStopPost` tears the whole session
down) → `pkexec` helper:
1. `unshare(CLONE_NEWNS)`, `/` rslave → this is the **workspace**.
2. `cryptsetup open A` (global dm device) → idmap-mount at `/vaults/<labelA>`.
3. Record A's `dm_name` + mountpoint in the **session lock** (`/run/veracage/
   session-<sid>.lock`, root-owned 0600, one line per open volume).
4. Drop privileges → exec the session leader in the workspace NS. The leader
   writes `/run/veracage/session-<sid>.pid` (root-readable) and holds the NS.

### Subsequent open (add a volume to the running session)
`veracage open <B>` → a **short-lived** transient action (no ExecStopPost — B's
teardown is owned by the session, not this action) → `pkexec` helper:
1. Read `session-<sid>.pid`, verify it is the live veracage-uid session leader
   (comm + uid + start-time, defeating pid reuse), then
   `setns(/proc/<pid>/ns/mnt)` into the workspace.
2. `cryptsetup open B` → idmap-mount at `/vaults/<labelB>` **in the workspace**
   (`move_mount` lands it there).
3. Append B to the session lock; signal the leader to re-seed Places.
4. Exit. B now belongs to the session; its dm device is closed only at session
   teardown (or an explicit per-volume close).

### Close
- **One volume**: `veracage close <B>` → leader (or a small root action) unmounts
  `/vaults/<labelB>` + `cryptsetup close` B's dm; drop B from the session lock.
- **Session** (compositor window closed, or crash): leader exits → anchor unit
  stops → `ExecStopPost` cleanup walks the session lock and closes **every** dm +
  unmounts every `/vaults/*`.

## What must be nailed (spike these first)

1. **idmap-into-setns.** ✅ **PROVEN — `prototype/spike8_setns_idmap.py`, VPS
   (Debian 13, kernel 6.12), 16/16 checks green (2026-07-08).** A root child
   `setns()`es into a dropped-uid holder's private mount NS and runs the exact
   `open_tree`/`mount_setattr(MOUNT_ATTR_IDMAP)`/`move_mount` dance the helper
   uses, landing two loop-backed volumes at `/vaults/<label>` in the joined NS.
   Verified: mounts land in the workspace NS and NOT in host `/proc/mounts`; the
   idmap holds (vault uid reads, on-disk/human uid denied); one process does a
   cross-volume copy (the DnD primitive); one `bwrap --bind /vaults` sees both
   volumes; killing the holder releases both mounts (loop devices detach). The
   privileged core is buildable as designed.
2. **Sandbox sees all volumes.** bwrap `--bind /vaults /vaults` (recursive) from
   the leader's NS. Apps see volumes open **at launch time**; a volume opened
   *after* an app started won't appear in it (its mount NS is fixed at launch) →
   for DnD, open both volumes first, then launch the file manager. Document this,
   or investigate mount propagation into the sandbox if live pickup is wanted.
3. **Session lock tracks all volumes** (append on add, drop on close) so crash
   cleanup closes every dm. Replaces the per-vault lock.
4. **Label → path** with dedup: `/vaults/<label>`, `…-2` on collision, short-hash
   fallback for empty/unsafe labels (reuse `_sanitize_label`).
5. **pid-verify before setns** — never join a namespace by an unverified pid.

## Security analysis

The boundary that matters is **unchanged**:
- Every volume is still presented as the `veracage` uid → the host and the
  same-uid human are still denied (deny-by-uid).
- Decrypted block devices are still `root:disk 0660` + `UDISKS_IGNORE`.
- Apps still run in bwrap: `--unshare-net`, no host filesystem, curated `/etc`,
  ephemeral XDG, `--clearenv`.
- The workspace NS is **private** → mounts stay out of the host `/proc/mounts`
  (the "hide" layer is preserved; this is why we `setns` rather than mount in the
  host NS).

What changes: **volume-from-volume isolation is dropped** — one bwrap sees all
open volumes at `/vaults/*`. That's the intended trade (co-hosted volumes are one
trust domain). New root capability introduced: the helper joins a *running*
namespace by pid → mitigated by strict pid verification + the session pidfile
being root-owned in root-owned `/run/veracage`.

## Rough phases

1. **Spike**: idmap-mount into a `setns`'d namespace on the VPS (prove item 1).
2. Session anchor + session leader + session lock (bootstrap path only, 1 volume).
3. Add-volume path (`setns` + mount + register), per-volume + session cleanup.
4. Sandbox `--bind /vaults`, one app set, multi-volume Places seeding.
5. `veracage close <volume>`; toolbar per-volume close affordance.
6. VPS end-to-end: two volumes, one Dolphin, DnD A↔B; crash cleanup closes both.

## Open decisions

- **Where the workspace NS lives**: the session leader (this note) vs. a dedicated
  holder process. Leader is simplest (it already anchors the session).
- **Live pickup** of volumes opened after an app launched (mount propagation) —
  or accept "open volumes first, then launch". Recommend the latter for v1.
- **Per-volume close UX** in the toolbar (needs the toolbar to know the volume
  set — a small extension of the `.apps` channel).
