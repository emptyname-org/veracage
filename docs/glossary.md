# Glossary — UID-isolation architecture

Working vocabulary for the deny-by-UID rebuild (the "B2" topology). **Naming is
provisional** — under review before publication (see *Open naming questions* at
the end; the product name itself is pending a rename now that it covers LUKS as
well as VeraCrypt).

Everything is organized around *who a process runs as*.

## The three domains

There are three uids in play:

- **root** (uid 0) — used for **seconds only**, by the helper, to do the
  privileged setup. Never runs your apps.
- **human** / **human uid** — *you* (e.g. `pq`, uid 1000): the real logged-in
  user. Has **no access to the decrypted vault** in this design — that is the
  whole point.
- **vault uid** / **vault user** — a dedicated system account named `veracage`
  (e.g. uid 999) that no person can log in as. The decrypted vault is owned by
  it, and your apps run as it. The "separate identity" the isolation hangs on
  (the desktop analogue of Android's per-app uid).

## The processes

- **launcher** — what `veracage open` runs as, in *your* (human) shell. Resolves
  the app, then kicks off the privileged setup. (`cmd_open` in `cli.py`.)
- **helper** — the small **Rust** binary (`helper-rs`) that runs as **root** via
  pkexec for a few seconds: opens the encrypted volume, sets up the idmap mount,
  then **drops to the vault uid** and hands off. (`helpers/veracage-helper`.)
- **continuation** — the program the helper `exec`s *after* dropping privileges.
  It **is** the leader. Called "continuation" because the helper continues into
  it. It is **pinned** at build time (baked into the helper binary) so a
  malicious direct pkexec call cannot substitute its own program.
- **leader** / **vault-side leader** — the long-lived session boss, running as
  the **vault uid** inside the private namespace. Attaches to the persistent
  nested compositor (it does not start it), launches apps, serves the control
  socket. (`leader.py`.)
- **agent** / **broker** — the human-side helper (`agent-rs`), running as *you*
  (human). **Windowless** — only transient dialogs; it does the host-side things
  a vault-uid process can't (`pkexec` the mount, host file dialogs, write config).
  Has *no* vault access.

## The isolation (why external processes can't read the vault)

- **deny-by-UID** — our model: the vault is *owned* by the vault uid, so anyone
  who isn't that uid is **denied by ownership** — even root, even a same-uid
  attacker. Fails *safe*.
- **hide-by-mount-namespace** — the **old** model (option-a) we are replacing:
  the vault was only *hidden*, readable by anything that got into the namespace.
  Fails *open*.
- **idmap** / **idmapped mount** — the kernel feature (≥ 5.12) that makes a
  filesystem *appear* owned by a different uid **without touching the data on
  disk**. How we present the vault (files owned by you, 1000) as owned by the
  vault uid (999).
- **mount namespace** (**NS**) — a private set of mounts. The decrypted vault
  mount exists **only inside** the leader's namespace, so host processes can't
  even *see* it.
- **bwrap** / **sandbox** — bubblewrap. Wraps each app: no network, no host
  filesystem, only `/vault` + the compositor socket visible. So even a malicious
  app **can't exfiltrate** what it reads.
- **the three barriers** — **deny** (idmap) + **hide** (namespace) + **sandbox**
  (bwrap). An escape of any one still does not yield plaintext.

## The display (cross-uid GUI)

- **the veracage compositor** — our own `veracage-compositor`, a **separate**
  compositor instance — **one persistent instance shared by every session**, on
  `/run/veracage/rt/wl-vc` — that runs as the vault uid. The sandboxed apps
  connect to *it*, not to your desktop session — so the sandbox clipboard /
  screencopy are separate from your desktop. It renders into a single window on
  your real compositor.

## Talking across the boundary

- **control socket** — the unix socket the leader listens on for
  `ping`/`list`/`close` — status/lifecycle only, **no** exec or file transfer
  (any same-uid process can reach it, so it must not be a vault-exfiltration
  lever). The human side connects to it *by path* (the helper created it in your
  runtime dir, owned by you).
- **exchange folder** — the shared host↔vault directory (host `~/Veracage/Exchange`
  by default, idmap-mounted into the sandbox at `/exchange`). Files dropped in on
  either side appear on the other, owned by you — the file-transfer path (the old
  control-socket bridge was removed).

## Privilege & lifecycle

- **pkexec** / **polkit** — the tool that runs the helper as root + the policy
  that governs it. Reachable by any active local user, so the helper trusts
  *nothing* from its arguments.
- **`PKEXEC_UID`** — the env var pkexec sets to the **real caller's** uid. The
  helper reads this to know "who the human is" — never from its own arguments.
- **systemd `--user` transient service** / **`ExecStopPost`** — the session is
  wrapped in a transient systemd **service** (not a scope — scope units reject
  `Exec*` properties) so that **cleanup runs even if everything is SIGKILL'd**
  (the `ExecStopPost` fires when the unit stops).
- **session lock** — `/run/veracage/session-<sid>.lock` (`<sid>` = the human
  uid); records every open volume's dm-device name so the crash-cleanup closes
  them all. A `session-<sid>.flock` sibling serializes open/close/teardown. (The
  legacy per-vault `<hash>.lock` remains for the deprecated single-vault path.
  Distinct from the future **lock *state*** — the lock/unlock *feature* that
  evicts the key for fast resume.)

## Crypto & mounts

- **cryptsetup** — the tool that decrypts; we shell out to it for both formats.
- **LUKS** / **VeraCrypt** — the two volume formats we support
  (`backend = luks | veracrypt | auto`).
- **dm device** — the decrypted block device cryptsetup creates at
  `/dev/mapper/veracage-<hex>`. Closing it = **evicting the key**.
- **workspace** / **mountpoint** — the shared-workspace tmpfs at
  `/run/veracage/vaults` (private to the session NS); each volume is idmap-mounted
  at `/vaults/<label>` inside it, bound into the sandbox as `/vaults`.
- **`.raw`** (staging) / **`.run`** (vault runtime) — dirs the helper provisions:
  `.<label>.raw` is the plain mount it reads to set up the idmap (dot-prefixed, so
  the leader ignores it); `session-<sid>.run` is the vault uid's writable scratch
  (compositor socket, bwrap's `/run/user`).

## Project shorthand

- **B2** — the topology we chose: the clean human ↔ vault-uid boundary (vs. the
  rejected variants in the rebuild plan).
- **the spikes** — the throwaway `prototype/spikeN.py` scripts, each proving one
  risky mechanism in isolation (idmap, cross-uid Wayland, the full helper flow).
- **the VPS** (`veracage-vps`) — the disposable Debian box where the privileged
  tests run.

## Open naming questions (for review)

- **Product name** — "Veracage" = VeraCrypt + cage. No longer fits (LUKS too,
  and there is no literal cage). Pending rename.
- **human** vs **user** — "human" is used to disambiguate from the *vault user*;
  "user" alone is ambiguous now that there are two.
- **continuation** — accurate but jargon-y; candidates: *handoff*, *leader-exec*.
- **leader** / **agent** / **helper** — process roles; check these read clearly
  in user-facing messages and docs.
- **vault** — used for the encrypted volume; fine, but confirm it does not
  clash with the *vault uid* phrasing in prose.
