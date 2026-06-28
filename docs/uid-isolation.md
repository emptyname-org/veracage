# UID isolation core — validated design

The rebuild isolates the decrypted vault by **deny-by-ownership**: the vault is
presented as owned by a dedicated **vault uid**, apps run as that uid, and the
human (and root, and any other init-ns uid) is denied by plain Unix perms.
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

## On-disk owner / portability

idmap reads the vault's stored ownership and presents it as the vault uid, so
**vaults stay portable with zero on-disk changes** — open on any machine
regardless of its local vault-uid value. The helper detects the vault's owner
at mount (stat the root after a plain mount) and maps `<that uid> → <vault uid>`.
Single-user vaults (the common case) have one owner; multi-owner vaults need a
range map (or are documented as out of scope).

## Topology (B2) this enables

- **root** (helper, seconds): `cryptsetup open` (LUKS|VeraCrypt) → idmap-mount as
  the vault uid in a private mount NS → spawn the vault-side leader as the vault uid.
- **vault uid**: nested weston (host socket via inherited fd), the apps, and a
  tiny `vault-io` helper for transfers. Can't reach the host; host can't read it.
- **human uid** (no vault access): supervisor, control socket, tray agent, host
  clipboard end. Transfers cross the boundary only via `vault-io`.

Bridge implication: the human agent **cannot** read the vault, so import/export
goes through the vault-uid `vault-io` helper over the control socket (shared
POSIX group) — which is what removes the old in-NS-agent soft spot.
