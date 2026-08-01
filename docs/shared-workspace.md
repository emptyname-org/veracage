# Shared workspace: N volumes, one sandbox

Multiple mounted volumes share one **workspace**: a single private mount
namespace holding every volume at `/vaults/<label>`, one session leader, one
set of apps. A single file manager sees all mounted volumes side by side, so
drag-and-drop between volumes is an ordinary in-process move. Cross-volume
isolation is explicitly not a goal (all volumes are the same user's own data,
see `uid-isolation.md`). Volume mounts stay hidden from the host
`/proc/mounts`. Builds on `uid-isolation.md` (deny-by-uid + the persistent
one-compositor architecture).

```
workspace mnt NS (private, hidden from host /proc/mounts)
  /vaults/<labelA>/     ← idmap mount of volume A (owner → veracage uid)
  /vaults/<labelB>/     ← idmap mount of volume B
  …
```

- **One session leader** runs in the workspace NS (as the veracage uid) and
  holds it open for the session's lifetime.
- **One app set** (the user's configured apps). An app launches in a bwrap
  that binds the whole `/vaults` tree. Apps see the volumes mounted **at launch
  time**. A volume mounted later does not appear in an already-running app
  (its mount NS is fixed at launch): mount the volumes first, then launch the
  file manager.
- The **compositor** is unchanged (one persistent instance, apps connect by
  socket). It does not need the mounts and stays in its own namespace.

Labels map to `/vaults/<label>`, `…-2` on collision, with a short-hash
fallback for empty or unsafe labels.

## Lifecycle

### First open (bootstrap)

`veracage open <A>` -> the CLI brings up the compositor as its own
`systemd --user` unit (if not already running, so it survives session
teardown), then wraps the mount in a transient unit (the **session anchor**,
its `ExecStopPost` tears the whole session down) -> `pkexec` helper:

1. `unshare(CLONE_NEWNS)`, `/` made rslave -> this is the **workspace**.
2. `cryptsetup open A` (global dm device) -> idmap-mount at `/vaults/<labelA>`.
3. Record A's `dm_name` + mountpoint in the **session lock**
   (`/run/veracage/session-<sid>.lock`, root-owned 0600, one line per open
   volume).
4. Drop privileges -> exec the session leader in the workspace NS. The leader
   writes `/run/veracage/session-<sid>.pid` and holds the NS.

### Subsequent open (add a volume)

`veracage open <B>` -> a short-lived transient action (no `ExecStopPost`: B's
teardown is owned by the session, not this action) -> `pkexec` helper:

1. Read `session-<sid>.pid`, verify it names the live veracage-uid session
   leader (comm + uid + start time, defeating pid reuse), then
   `setns(/proc/<pid>/ns/mnt)` into the workspace.
2. `cryptsetup open B` -> idmap-mount at `/vaults/<labelB>` in the workspace
   (`move_mount` lands it there).
3. Append B to the session lock, signal the leader to re-seed Places.

B now belongs to the session. Its dm device is closed at session teardown or
by an explicit per-volume close.

### Close

- **One volume**: `veracage close-volume <label>` (or **File > Dismount** in
  the menu) dismounts `/vaults/<label>`, `cryptsetup close`s its dm device, and
  drops it from the session lock. The rest of the session keeps running.
- **Session** (compositor window closed, or crash): the leader exits -> the
  anchor unit stops -> its `ExecStopPost` cleanup walks the session lock and
  closes every dm device + dismounts every `/vaults/*`.

## Security

The boundary is unchanged from the single-volume model:

- Every volume is presented as the `veracage` uid: the host and the same-uid
  human are denied (deny-by-uid).
- Decrypted block devices stay `root:disk 0660` + `UDISKS_IGNORE`.
- Apps run in bwrap: `--unshare-net`, no host filesystem, curated `/etc`,
  ephemeral XDG, `--clearenv`.
- The workspace NS is private: mounts stay out of the host `/proc/mounts`
  (this is why the helper `setns`es into the workspace rather than mounting in
  the host NS).

What differs from a per-volume namespace model: volume-from-volume isolation
is dropped. One bwrap sees all mounted volumes at `/vaults/*`. That is the
intended trade (co-hosted volumes are one trust domain). One root capability
is specific to this design: the helper joins a running namespace by pid,
mitigated by the strict pid verification above and by the session pidfile
being root-owned in root-owned `/run/veracage`.
