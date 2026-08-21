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

`veracage open <B>` -> a short-lived transient action (it carries the same
`ExecStopPost` as the bootstrap, which no-ops while the leader is alive: B's
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

- **One volume**: `veracage close-volume <label>` (or **File > Close volume** in
  the menu) unmounts `/vaults/<label>`, `cryptsetup close`s its dm device, and
  drops it from the session lock. The rest of the session keeps running.
  **File > Close volume > All** is the same operation per volume, all of them in
  one write to the command file (a verb per line, which the broker reads whole).
  The close is REAL: a plain `umount` and a plain `cryptsetup close`, no
  `MNT_DETACH` and no `--deferred`, so the key is out of RAM by the time the
  helper exits rather than "later, when an app lets go". A volume that cannot be
  closed is therefore never half-dismounted: the helper checks first and touches
  nothing until the check passes, leaving the volume mounted, in the lock, and in
  the menu.

  **What holds it is a running app, not an open file.** Each app's bwrap sandbox
  binds the workspace **recursively**, so the volume's mount lives in that app's
  mount namespace either way, and `cryptsetup close` answers "Device is still in
  use". Measured: an idle app and an app with a file open are
  indistinguishable, and `umount` succeeds in BOTH - so VeraCrypt's own busy
  test (run plain `umount`, report its failure, see
  `CoreUnix::DismountFilesystem`) would detect nothing here. The reliable test,
  and what `foreign_holders` implements, is counting holders of the device in
  mount namespaces other than the leader's: it predicted the `cryptsetup close`
  outcome exactly in every measured case, driving the real helper end to end:
  refused with the app running and nothing changed, closed for real once it is
  gone.

  **One close, one password.** The helper does not hand the problem back: it
  prints `holders\t<names>` and WAITS on its stdin, still authenticated, still in
  the session's namespace. The broker puts the one question in front of the human
  ("Close <apps> to continue.") and answers for them:

  - **Close** has the compositor ask every app window to close (unsaved work
    still gets its own prompt), then has the leader stop whatever is left
    (`close-apps` on the toolbar socket, which keeps the session running). The
    leader's `closeapps.done` makes the broker write `go` to the waiting helper,
    which checks the holders again and closes the volume for real.
  - **Cancel** closes the helper's stdin. The EOF makes it exit `6`
    (`EXIT_VOLUME_BUSY`) with a `busy\t<reason>` line, having touched nothing.

  Waiting rather than retrying is what keeps it to one prompt: polkit's
  `auth_self_keep` is bound to the CALLING PROCESS, so a second attempt is a
  second `pkexec` from a second process and asks for the password again. The
  helper's wait is bounded (`HOLDERS_WAIT`, 300s) so a caller that dies cannot
  park a root process in the session's namespace.
- **Session** (Quit, the window's close button, or a crash). Veracage must never
  disappear while a volume is still decrypted, so the window stays up for the
  whole sequence and the exit is gated on the volumes actually being closed, not
  on a timer:
  1. **Ask.** The compositor sends every app window the same close request its
     own title-bar X sends, so an app with unsaved work raises its own prompt and
     the human answers it in the still-open Veracage window. Nothing is killed
     and nothing is dismounted yet. **No time limit and no override:** quitting
     again only asks the windows again, so an app that keeps its window up keeps
     Veracage open. That is an unanswered prompt, not a hang.
  2. **Enforce.** Only once NO app window is left are the sessions told to close
     on their app sockets. Each leader SIGTERMs whatever is still running (a
     windowless helper process), gives it `_TERMINATE_GRACE`, SIGKILLs the rest,
     and exits. Nothing that had a window is killed here: they all agreed in 1.
  3. **Close.** The leader's exit takes its mount namespace with it (and every
     `/vaults/*` mount), the anchor unit stops, and its `ExecStopPost` cleanup
     walks the session lock and closes every dm device. The cleanup retries a
     close the kernel reports busy for `CLOSE_RETRY_FOR`: it runs moments after
     the leader exited and the namespace is freed asynchronously, so a single
     attempt can lose a race it only has to wait out.
  4. **Exit.** The compositor leaves when the session lock is gone, which the
     cleanup unlinks only after every device is closed. There is no banner
     (quitting is not news, and the window going away when it is done is the
     feedback), so a teardown that drags looks like a window that has not closed
     yet. It never exits early instead.

  This is also why a full quit can close volumes that **File > Close volume** cannot:
  the apps are gone by step 3, so nothing holds the device (see below).

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
