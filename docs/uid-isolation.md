# UID isolation core

Veracage isolates the decrypted volume by **deny-by-ownership**: the volume
is presented as owned by a dedicated **veracage uid**, apps run as that uid,
and the human (and any other **non-root** uid) is denied by plain Unix
permissions (root's residual access is bounded in *Threat model* below).
Validated on kernel 6.1 / util-linux 2.38. util-linux there is too old for
the `mount -o X-mount.idmap` CLI, so the helper uses the syscalls directly.

## The idmap recipe (exact)

To present a mount's on-disk owner as the veracage uid **without touching
the data** (no chown):

1. Create a transient user namespace whose maps are
   **`<on-disk-owner> <vault-uid> 1`** for both uid and gid
   (i.e. `inside = on-disk owner`, `outside = presented/vault uid`). Write
   `setgroups=deny`, then `uid_map`, then `gid_map` from the (root) parent.
2. `open_tree(AT_FDCWD, source_mount, OPEN_TREE_CLONE|OPEN_TREE_CLOEXEC|AT_RECURSIVE)`
   -> detached mount fd.
3. `mount_setattr(mnt_fd, "", AT_EMPTY_PATH, &{attr_set=MOUNT_ATTR_IDMAP, userns_fd}, sizeof)`.
4. `move_mount(mnt_fd, "", AT_FDCWD, target, MOVE_MOUNT_F_EMPTY_PATH)`.
5. Close the userns fd. The mount keeps the idmapping.

Requires kernel >= 5.12 (ext4 idmap). The direction is the subtle bit: mapping
`<vault> <on-disk> 1` (the intuitive way) maps the on-disk owner to *nobody*
and denies everyone.

## Access model

- Apps run **as the veracage uid in the init namespace** (an in-process
  `initgroups`/`setresgid`/`setresuid` drop in the helper, then
  `bwrap`). They are *not* inside the idmap userns: the relabel is a property
  of the mount, so a plain init-ns veracage-uid process reads it.
- The human, root, and any other init-ns uid are **denied**. The file's
  effective owner is the veracage uid, which they are not (even root: idmapped
  mounts strip its `CAP_DAC_OVERRIDE` for unmapped ids).
- A same-uid attacker **cannot become the veracage uid** (needs privilege) and
  **cannot join** a root-owned userns to get the inside view (it cannot even
  `open` `/proc/<pid>/ns/user`).

Three independent layers: **deny** (veracage-uid ownership via idmap) +
**hide** (private mount NS) + **sandbox** (bwrap unshares net/pid/ipc/...).

## Threat model: the boundary

**Defended: any non-root process, including one running as the human's own
uid.** It can't read the volume *files* (idmapped to the veracage uid: even
the human, the on-disk owner, is denied through the mount), can't *become*
the veracage uid (needs privilege), and can't reach the decrypted *block
device*: `/dev/mapper/veracage-*` is `root:disk 0660` and hidden from UDisks
(`UDISKS_IGNORE`), so there is no unprivileged path (direct `open` or a
desktop "mount this drive" click) to the plaintext. The one channel it *can*
reach, the human-owned control socket, carries status and the enabled-app
list only (`ping`/`list`/`close`/`set-apps`): no command execution and no
file transfer, so it is not an exfiltration path either. `set-apps` only
replaces which apps the menu offers, the same human-trust data as
`config.toml` (its source), and cannot itself launch anything. This is the real threat (malware / curious
apps running as the user), and it is sealed.

**Not defended: local root.** Root can `sudo mount /dev/mapper/veracage-*` and
read the plaintext, or scrape the memory of any app that has the volume open.
This is inherent: root reads everything, and deny-by-uid stores the human's
*own* data. No software layer closes it: a normal VM doesn't either (host root
reads the guest's RAM via `/proc/<qemu>/mem` and the backing disk). Only
confidential-computing *hardware* (AMD SEV-SNP / Intel TDX) seals a guest from
host root, which needs server-class silicon and is out of scope for a desktop
tool. On a single-user desktop root *is* the human: an attacker who already
has root has the passphrase and the decrypted RAM regardless, VM or not.

**Not a goal: isolating volumes from each other.** The one compositor hosts
every open volume's apps and shares one clipboard/selection between them.
That's intentional: all volumes are the same human's own data. Partitioning
selections per-volume would add real complexity to defend the human against
themselves. Co-hosted volumes are one trust domain.

**Not a goal: confining the app you chose to run.** The boundary is
one-directional. It keeps the host out of the volume, not the app out of the
host. The app you launch is trusted, so a malicious sandboxed app is outside
the threat model (not a secondary threat, just outside). The sandbox
restrictions (no network, no host filesystem) are there to keep the decrypted
data from leaking out, not to cage the app.

## On-disk owner / portability

idmap presents the volume's files as owned by the veracage uid without
rewriting them, so the volume opens on any machine regardless of its local
veracage-uid value.

The mapping source is the **calling human's uid**, not a probe of the volume:
`mount_volume_at_workspace` chowns the staged root to the caller (best effort,
so a read-only or ownerless filesystem still mounts) and passes `human_uid` to
`idmap_mount`. Files owned by any OTHER uid are therefore not mapped and appear
as `nobody` inside the sandbox: a volume written on another machine under a
different uid is readable only where its files are world-readable. Detecting the
on-disk owner (stat the staged root) and mapping that instead would remove the
restriction, and a multi-owner volume would need a range map. Both are out of
scope today.

## Process architecture

A single persistent **`veracage`-uid compositor** is the shell. Volumes are
sources of apps loaded into it. A thin **human-uid broker** does the one thing
a `veracage`-uid process cannot (`pkexec`) and mediates host files.

```
  human uid   broker (.desktop / CLI)
                • Mount volume -> pkexec helper (mount)
                     │ pkexec
                     ▼
  root        helper (seconds): cryptsetup open (LUKS|VeraCrypt) → fsck -p the
              decrypted filesystem → idmap-mount as the veracage uid into the
              private workspace NS → drop to veracage → exec the session leader.
                     │
  veracage    compositor (persistent, ONE): renders every volume's apps into
  uid         one host window, owns the clipboard, hosts the egui menu bar.
              session leader (one per session): holds the workspace NS,
              launches apps in bwrap wired to /run/veracage/rt/wl-vc, serves
              the control socket.
  root        a static systemd system-sleep hook dismounts before suspend.
```

- **Why the `veracage` uid seals it.** `veracage` is a *standing system user*
  (created at install), so the helper can spawn a veracage-uid compositor
  before any volume: no forced human-uid, no `ptrace_scope` regression, no
  cross-uid fd-passing/ACLs. Everything holding decrypted content (compositor
  buffers/textures/clipboard source, the apps) runs as `veracage`, a different
  uid than the human, so a same-uid process can't `ptrace` it or read its
  `/proc/<pid>/mem` (needs `CAP_SYS_PTRACE`). The one human-uid capability
  that survives (and the reason the broker exists) is `pkexec`.
- **Compositor owns the clipboard.** It owns the sandbox selection and reaches
  the host clipboard as a wlr-data-control client on its own winit->host
  connection, so both sides live in one process (no cross-process relay).
  Paste in = host->sandbox, Copy out = sandbox->host, and the toolbar's
  clipboard buttons are in-process.
- **Control socket = status + app list** (`ping`/`list`/`close`/`set-apps`).
  App launches happen over a separate veracage-owned toolbar socket by index
  into the human's own enabled list, and `set-apps` only updates that list
  (human-trust data, like `config.toml`). Any host-file import/export runs on the
  human-driven compositor/broker path (which other-uid processes can't reach),
  gated by a real user action, never the control socket.
- **Persistence.** The compositor is spawned once on the fixed
  `/run/veracage/rt/wl-vc` socket and outlives the session leader: the leader
  dies on session close, the compositor does not.
- **Residual** (inherent to nesting): the host compositor (and a human-uid
  screencopy tool) can capture whatever sandbox content is *visible on
  screen*. Only the on-screen pixels live in the host compositor's output,
  never the nested compositor's offscreen memory.
