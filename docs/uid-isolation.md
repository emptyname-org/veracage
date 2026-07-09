# UID isolation core — validated design

The rebuild isolates the decrypted vault by **deny-by-ownership**: the vault is
presented as owned by a dedicated **vault uid**, apps run as that uid, and the
human — and any other **non-root** uid — is denied by plain Unix perms (root's
residual access is bounded in *Threat model* below).
Proven end-to-end by the `prototype/` spikes on **kernel 6.1 / util-linux 2.38**
(util-linux is too old for the `mount -o X-mount.idmap` CLI, so we use the
syscalls directly — which the helper does anyway).

## The idmap recipe (exact)

To present a mount's on-disk owner as the vault uid **without touching the
data** (no chown):

1. Create a transient user namespace whose maps are
   **`<on-disk-owner> <vault-uid> 1`** for both uid and gid
   (i.e. `inside = on-disk owner`, `outside = presented/vault uid`). Write
   `setgroups=deny`, then `uid_map`, then `gid_map` from the (root) parent.
2. `open_tree(AT_FDCWD, source_mount, OPEN_TREE_CLONE|OPEN_TREE_CLOEXEC|AT_RECURSIVE)`
   → detached mount fd.
3. `mount_setattr(mnt_fd, "", AT_EMPTY_PATH, &{attr_set=MOUNT_ATTR_IDMAP, userns_fd}, sizeof)`.
4. `move_mount(mnt_fd, "", AT_FDCWD, target, MOVE_MOUNT_F_EMPTY_PATH)`.
5. Close the userns fd — the mount keeps the idmapping.

Requires kernel ≥ 5.12 (ext4 idmap). The direction was the subtle bit: mapping
`<vault> <on-disk> 1` (the intuitive way) maps the on-disk owner to *nobody*
and denies everyone — see spikes 3/4.

## Access model (why it's secure)

- Apps run **as the vault uid in the init namespace** (`setpriv` + `bwrap`).
  They are *not* inside the idmap userns — the relabel is a property of the
  mount, so a plain init-ns vault-uid process reads it. (spike5)
- The human, root, and any other init-ns uid are **denied** — the file's
  effective owner is the vault uid, which they are not. (spike3: even root is
  denied — idmapped mounts strip its `CAP_DAC_OVERRIDE` for unmapped ids.)
- A same-uid attacker **cannot become the vault uid** (needs privilege) and
  **cannot join** a root-owned userns to get the inside view (spike4: the human
  couldn't even `open` `/proc/<pid>/ns/user`). → threat 1.1.1 satisfied.

Three independent layers remain: **deny** (vault-uid ownership via idmap) +
**hide** (private mount NS) + **sandbox** (bwrap unshares net/pid/ipc/…).

## Threat model — the boundary

**Defended: any non-root process, including one running as the human's own uid.**
It can't read the vault *files* (idmapped to the vault uid — even the human, the
on-disk owner, is denied through the mount), can't *become* the vault uid (needs
privilege), and can't reach the decrypted *block device*: `/dev/mapper/veracage-*`
is `root:disk 0660` and hidden from UDisks (`UDISKS_IGNORE`), so there is no
unprivileged path — direct `open` or a desktop "mount this drive" click — to the
plaintext. The one channel it *can* reach, the human-owned control socket, carries
status only (`ping`/`list`/`close`) — no command execution and no file transfer,
so it is not an exfiltration path either. (An earlier `exec`/`export` bridge on
that socket *was* such a path; a pen test read the whole vault through it, and it
was removed — see `fixed-problems.md`.) This is the real threat (malware /
curious apps running as you), and it is sealed.

**Not defended: local root.** Root can `sudo mount /dev/mapper/veracage-*` and read
the plaintext, or scrape the memory of any app that has the vault open. This is
inherent — root reads everything, and deny-by-uid stores the human's *own* data. No
software layer closes it: a normal VM doesn't either (host root reads the guest's
RAM via `/proc/<qemu>/mem` and the backing disk); only confidential-computing
*hardware* (AMD SEV-SNP / Intel TDX) seals a guest from host root, which needs
server-class silicon and is out of scope for a desktop tool. On a single-user
desktop root *is* the human — an attacker who already has root has the passphrase
and the decrypted RAM regardless, VM or not.

**Not a goal: isolating vaults from each other.** The one compositor hosts every
open vault's apps and shares one clipboard/selection between them. That's
intentional — all vaults are the same human's own data, opened one at a time;
partitioning selections per-vault would add real complexity to defend the human
against themselves. Co-hosted vaults are one trust domain.

## On-disk owner / portability

idmap reads the vault's stored ownership and presents it as the vault uid, so
**vaults stay portable with zero on-disk changes** — open on any machine
regardless of its local vault-uid value. The helper detects the vault's owner
at mount (stat the root after a plain mount) and maps `<that uid> → <vault uid>`.
Single-user vaults (the common case) have one owner; multi-owner vaults need a
range map (or are documented as out of scope).

## Process architecture (as built)

A single persistent **`veracage`-uid compositor** is the shell; vaults are sources
of apps loaded into it. A thin **human-uid launcher** does the one thing a
`veracage`-uid process cannot — `pkexec` — and mediates host files.

```
  human uid   launcher (.desktop / CLI)
                • Open vault → pkexec helper (mount)
                • host-file import/export (deferred; on the compositor path)
                     │ pkexec
                     ▼
  root        helper (seconds): cryptsetup open (LUKS|VeraCrypt) → idmap-mount as
              the vault uid in a private mount NS → drop to veracage → exec leader;
              also spawns the compositor (as veracage) on first open.
                     │
  veracage    compositor (persistent, ONE): renders every vault's apps into one
  uid         kwin window, owns the clipboard, hosts the egui toolbar.
              leader (per vault): holds the mount NS, launches apps in bwrap wired
              to /run/veracage/rt/wl-vc, serves the control socket.
  root        a static systemd system-sleep hook dismounts before suspend.
```

- **Why `veracage`-uid seals it.** `veracage` is a *standing system user* (created
  at install), so the helper can spawn a veracage-uid compositor before any vault —
  there is no forced human-uid, no `ptrace_scope` regression, no cross-uid
  fd-passing/ACLs. Everything holding decrypted content (compositor
  buffers/textures/`clip_source`, the apps) runs as `veracage`, a different uid than
  the human, so a same-uid process can't `ptrace`/read its `/proc/<pid>/mem` (needs
  `CAP_SYS_PTRACE`). The one human-uid capability that survives — and the reason the
  launcher exists — is `pkexec`.
- **Compositor owns the clipboard.** It owns the sandbox selection and reaches the
  host clipboard through its own winit→kwin `smithay-clipboard` connection, so both
  sides live in one process — no cross-process relay. Push = host→sandbox selection;
  pull = sandbox→host; the toolbar's clipboard buttons are in-process.
- **Control socket = status only** (`ping`/`list`/`close`). App launches happen over
  a separate veracage-owned toolbar socket by index into the human's own enabled
  list. Host-file **import/export** must run on the human-driven compositor/launcher
  path (which other-uid processes can't reach), gated by a real user action — never
  the control socket (the old bridge there was an exfil hole; removed). Deferred.
- **Persistence.** The compositor is spawned once on the fixed
  `/run/veracage/rt/wl-vc` socket and outlives every leader; leaders die on
  vault-close, the compositor does not.
- **Cross-vault** filesystem isolation holds (per-leader mount NS) and
  process/memory isolation holds (per-app bwrap pid ns); the shared clipboard is
  deliberate (one trust domain). Residual (inherent to Mode B nesting): kwin — and a
  human-uid screencopy tool — can capture whatever sandbox content is *visible on
  screen*; only the on-screen pixels live in kwin's output, never the compositor's
  offscreen memory.
