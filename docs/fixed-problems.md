# Fixed problems

Security and correctness issues that have been **found, fixed, and verified**
(regression suite green + targeted checks / spikes on the dev VPS). Grouped by the
review round that found them; most-severe first within each. The still-open
counterparts live in `known-problems.md`.

---

## Round 1 — initial hardening + Phase 2–4 review (2026-07-07)

- **[CRITICAL] Same-uid vault exfiltration via the control socket.** The
  human-owned control socket (`$XDG_RUNTIME_DIR/veracage/sessions/<h>.sock`,
  reachable by ANY process running as the human) exposed `exec` (run an arbitrary
  command in the vault sandbox as the vault uid) plus `import`/`export`/`outbox`
  (fd-passing files across the boundary). A live pen test read the whole mounted
  vault as an unprivileged uid-1000 process: `exec sh -c 'tar … > outbox'` →
  `export` → plaintext on the host — no passphrase, no root. Every *direct* path
  (device, idmapped mount, process memory, mount NS, procfs) was correctly
  blocked; this was the one hole. Fix: the control socket now carries only
  `ping`/`list`/`close`; launching is done solely over the veracage-owned toolbar
  socket by index into the human's own enabled list (`_accept_app_launch`), which
  other uids can't reach; the socket file bridge was removed outright (leader,
  `agent-rs/proto`, `cli exec`, `transfer.py`). Also removed the hardcoded app
  catalog — the user enables any installed binary — since the fix made clear the
  boundary is *who may drive the leader*, not *which app runs*.
- **[CRITICAL] Root privilege escalation.** `helper-rs/ipc.rs`
  `create_control_socket` used caller-supplied `XDG_RUNTIME_DIR` (from `--setenv`)
  as root to `create_dir_all` + `chmod`/`chown` **following symlinks**. A local
  user + a crafted vault + `XDG_RUNTIME_DIR` pointing at a dir with
  `veracage/sessions → /etc/cron.d` → root chowns an arbitrary path. Fix: validate
  `== /run/user/<uid>` (main.rs) + `lchown` + `create_dir_no_symlinks` (ipc.rs).
- **[HIGH] Decrypted vault appeared as a host drive.** `cryptsetup open` creates a
  GLOBAL `/dev/mapper/veracage-<hex>` (dm devices aren't mount-namespaced), so
  UDisks/Solid enumerated it and offered it in the desktop drive menu; a click
  mounted the RAW device on the host (polkit auth), bypassing the sandbox. Fix:
  `install/99-veracage.rules` sets `UDISKS_IGNORE=1` on `veracage-*` dm devices;
  the node is `root:disk 0660` so a non-root, non-`disk` user can't open it
  directly either. Residual (inherent): `sudo mount` as root still works.
- **[HIGH] Serve-loop teardown.** `leader.py` `_accept_one`/`_accept_app_launch`
  `accept()` sat outside the guard; a transient `ECONNABORTED`, or any
  non-`FileNotFoundError` launch error, unwound to the `finally` and SIGKILLed
  every app. Fix: try/except around accept+dispatch.
- **[HIGH] Launcher fire-and-forget.** `ui_launcher.rs` spawned `veracage open`
  without waiting → no error feedback on a failed mount + a zombie per session.
  Fix: track children, poll `try_wait`, reap + report fast failures.
- **[HIGH] Pre-check unlinks a live session.** `cli.py` treated a probe **timeout**
  (an `OSError` subclass) as a stale socket and unlinked it → orphan + double
  mount. Fix: `ConnectionRefusedError` = stale (unlink); timeout/other = refuse.
- **[MED] VeraCrypt GUI unlock broken.** `crypt.rs` pushed `--key-file=-` for both
  backends, but for tcrypt that's a *keyfile*, not the passphrase. Fix: pipe the
  passphrase to stdin, no `--key-file` (LUKS + VC, newline-tolerant).
- **[MED] Crafted volume-label desync.** An fs label with a newline shifted the
  `.apps` protocol (wrong app launches) / control chars broke the XBEL. Fix:
  `_sanitize_label` (strip non-printables, cap 64).
- **[MED] Non-atomic config save.** A torn read during save → "no apps" → auto-
  detect overwrote the user's selection. Fix: temp+rename (config.py + config.rs).
- **[MED] ExecStopPost spaced path.** An install path with a space split the
  property → dismount never runs. Fix: quote the cleanup path.
- **[MED] `launch_app` froze the event loop.** Blocking connect+write on the
  single calloop thread. Fix: run it on a short-lived thread.
- **[MED] `scan_leaders` hang/OOM/traversal.** Unbounded `read_to_string` on
  attacker-named `*.apps` (FIFO/huge/loop), and `runtime.join(sockname)` trusted an
  absolute/`..` line 0. Fix: regular-file + 64 KB cap + reject non-relative sock.
  (A TOCTOU residual remains — see `known-problems.md`.)
- **[MED] Stuck grab.** A button release over the toolbar strip was swallowed →
  window glued to cursor. Fix: don't gate pointer events while a grab is active.
- **[MED] Render-loop `.unwrap()`.** A transient EGL/GL error aborted the whole
  compositor. Fix: log + skip the frame.
- **[MED] Liveness pid-reuse.** `compositor_is_up` was identity-free. Fix: check
  `/proc/<pid>/comm` starts with `veracage`.
- **Passphrase heap-scrape (launcher).** A pen test read the passphrase out of the
  launcher's heap as a plain uid-1000 process (`ptrace_scope=0`; `mem::take` + drop,
  no wipe). Fix: `Zeroizing<String>` with pre-reserved capacity, `.zeroize()`d on
  every exit path, and egui's undo `TextEditState` reset so its plaintext snapshots
  don't linger. (Helper-side heap copy is still open — see `known-problems.md`.)

---

## Round 2 — 4-agent from-scratch audit (2026-07-09)

Verified: regression suite green + `spike9` (real LUKS dm) green.

- **[HIGH] Passphrase-pipe hijack via `VERACAGE_CLI`.** `agent-rs/ui_launcher.rs`
  `veracage_bin()` honored `$VERACAGE_CLI` **unconditionally**, before the trusted
  `current_exe`-sibling resolution, and `do_mount` pipes the plaintext passphrase
  to that binary's stdin. A same-uid attacker persisting `VERACAGE_CLI=/path/evil`
  (`environment.d`, `systemctl --user set-environment`, shell rc) receives the
  passphrase on the next GUI mount → full offline vault break. Same class as the
  already-fixed `$PATH` hijack, through the one channel that fix missed. Fix: the
  env override is gated behind `#[cfg(debug_assertions)]` (installs are `--release`)
  and resolution fails closed (returns `Option`, no bare-`"veracage"` `$PATH`
  fallback); `do_mount` refuses the mount and wipes the passphrase if unresolved.
- **[HIGH] Clipboard `receive` OOM/panic.** `handlers/mod.rs` `send_selection`
  cloned the ≤16 MiB `clip_source` Vec and `thread::spawn`ed a writer per
  `wl_data_offer.receive`, unbounded and via the panicking spawn. After one ordinary
  user paste a hostile app loops `receive` → GB/s main-thread allocation +
  thousands of threads/fds → loop stall, fd/thread exhaustion, or `spawn` ENOMEM
  **panic** that kills the compositor and every co-hosted vault's apps. Fix:
  `clip_source: Option<Arc<Vec<u8>>>` (refcount bump, no per-request copy); a new
  `spawn_clip_worker` caps in-flight transfers at 16 (`AtomicUsize`) and uses
  `thread::Builder::spawn` with drop-on-`Err`; applied to `send_selection` +
  `pull_to_host`, and `launch_app` switched to `Builder::spawn` too.
- **[MED] Sandbox app stdio inherited → journal leak.** `leader.py` `_launch_app`
  `Popen` didn't redirect stdio, so viewers inherited the leader's terminal/journal
  and printed opened `/vault` paths there (unencrypted, same-uid-readable after the
  vault closes — an accidental-leak channel in the threat model). Fix:
  `stdin/stdout/stderr=DEVNULL` (test asserts the detach).
- **[MED] Suspend-kill misdirected by a spoofed leader.** `sleep_hook.py`
  `find_leader_pid` matched only on `/proc/*/cmdline` (no uid check) and
  `/run/veracage` is 0755, so a same-uid decoy with a forged `_leader` argv could
  take the SIGTERM/SIGKILL while the real leader survived → `cryptsetup close`
  EBUSY → the dm-crypt key stays in RAM across sleep. Fix: require the matched pid
  to be owned by the veracage uid (`_leader_uid` via `getpwnam`); +3 unit tests.
