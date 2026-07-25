# Glossary

Working vocabulary for the deny-by-UID architecture. Everything is organized
around *who a process runs as*.

## The three domains

Three uids are in play:

- **root** (uid 0) - used for **seconds only**, by the helper, to do the
  privileged setup. Never runs apps.
- **human** / **human uid** - the real logged-in user (e.g. uid 1000). Has
  **no access to the decrypted volume** in this design. That is the whole
  point.
- **veracage uid** / **veracage user** - a dedicated system account named
  `veracage` that no person can log in as. The decrypted volume is owned by
  it, and the apps run as it. The "separate identity" the isolation hangs on
  (the desktop analogue of Android's per-app uid). Code identifiers still
  call it `vault_uid`.

## The processes

- **launcher** - what `veracage open` runs as, in the human's shell. Resolves
  the app, then kicks off the privileged setup. (`cmd_open` in `cli.py`.)
- **helper** - the small **Rust** binary (`helper-rs`) that runs as **root**
  via pkexec for a few seconds: opens the encrypted volume, sets up the idmap
  mount, then **drops to the veracage uid** and hands off.
- **continuation** - the program the helper `exec`s *after* dropping
  privileges (it continues into it). It **is** the leader. **Pinned** at
  build time (baked into the helper binary) so a malicious direct pkexec call
  cannot substitute its own program.
- **leader** / **session leader** - the long-lived session boss, running as
  the **veracage uid** inside the private workspace namespace. Attaches to
  the persistent nested compositor (it does not start it), launches apps,
  serves the control socket. (`leader.py`.)
- **agent** / **broker** - the human-side helper (`agent-rs`), running as the
  human. **Windowless**, only transient dialogs. It does the host-side
  things a veracage-uid process can't (`pkexec` the mount, host file dialogs,
  write config) and publishes the configured app list + icons for the
  compositor's Apps menu. Has *no* volume access.

## The isolation

- **deny-by-UID** - the model: the volume is *owned* by the veracage uid, so
  anyone who isn't that uid is **denied by ownership**. Fails *safe* (an
  attacker who defeats the hiding still gets "Permission denied").
- **idmap** / **idmapped mount** - the kernel feature (>= 5.12) that makes a
  filesystem *appear* owned by a different uid **without touching the data on
  disk**. How the volume (files owned by the human on disk) is presented as
  owned by the veracage uid.
- **mount namespace** (**NS**) - a private set of mounts. The decrypted
  volume mount exists **only inside** the leader's namespace, so host
  processes can't even *see* it.
- **bwrap** / **sandbox** - bubblewrap. Wraps each app: no network, no host
  filesystem, only `/vaults` + the compositor socket visible. This keeps the
  decrypted data from leaking out to the network or host filesystem. It
  protects the data, not the host from the app: a malicious app is outside the
  threat model.
- **the three barriers** - **deny** (idmap) + **hide** (namespace) +
  **sandbox** (bwrap). An escape of any one still does not yield plaintext.

## The display

- **the veracage compositor** - `veracage-compositor`, a **separate** nested
  compositor, **one persistent instance shared by every session**, on
  `/run/veracage/rt/wl-vc`, running as the veracage uid. Sandboxed apps
  connect to *it*, not to the desktop session, so the sandbox clipboard /
  screencopy are separate from the desktop's. It renders into a single
  window on the real compositor.

## Talking across the boundary

- **control socket** - the unix socket the leader listens on for
  `ping`/`list`/`close`/`set-apps`. Status and the enabled-app list only,
  **no** exec or file transfer (any same-uid process can reach it, so it must
  not be a volume-exfiltration lever).
- **shared directory** - the directory both the host and Veracage see (host
  `~/Veracage/Exchange` by default, idmap-mounted into the sandbox at
  `/exchange`, `exchange` in config/code). Files dropped in on either side
  appear on the other, owned by the human. The file-transfer path.
- **pub dir** - `/run/veracage/pub`, root-created and human-owned: where the
  human side publishes the configured app list (`config.apps`) and menu
  icons for the compositor's Apps menu, plus the `mimeapps.list` seed the
  leader copies into each sandbox (default-app associations). Same trust
  level as config.toml.

## Privilege & lifecycle

- **pkexec** / **polkit** - the tool that runs the helper as root + the
  policy that governs it. Reachable by any active local user, so the helper
  trusts *nothing* from its arguments.
- **`PKEXEC_UID`** - the env var pkexec sets to the **real caller's** uid.
  The helper reads this to know "who the human is", never from its own
  arguments.
- **systemd `--user` transient service** / **`ExecStopPost`** - the session
  is wrapped in a transient systemd **service** (not a scope - scope units
  reject `Exec*` properties) so that **cleanup runs even if everything is
  SIGKILL'd** (the `ExecStopPost` fires when the unit stops).
- **session lock** - `/run/veracage/session-<sid>.lock` (`<sid>` = the human
  uid). Records every mounted volume's dm-device name so the crash-cleanup
  closes them all. A `session-<sid>.flock` sibling serializes
  open/close/teardown.

## Crypto & mounts

- **cryptsetup** - the tool that decrypts. Both formats go through it. A
  failed decrypt (almost always a wrong passphrase) makes the helper exit
  with code 4, which the GUI turns into a re-prompt.
- **LUKS** / **VeraCrypt** - the two supported volume formats
  (`backend = luks | veracrypt | auto`).
- **dm device** - the decrypted block device cryptsetup creates at
  `/dev/mapper/veracage-<hex>`. Closing it = **evicting the key**.
- **workspace** - the shared-workspace tmpfs at `/run/veracage/vaults`
  (private to the session NS). Each volume is idmap-mounted at
  `/vaults/<label>` inside it, bound into the sandbox as `/vaults`.
- **`.raw`** (staging) / **`.run`** (runtime) - dirs the helper provisions:
  `.<label>.raw` is the plain mount it reads to set up the idmap
  (dot-prefixed, so the leader ignores it), and `session-<sid>.run` is the
  veracage uid's writable scratch (compositor socket, bwrap's `/run/user`).
