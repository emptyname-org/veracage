# Future features

Ideas deliberately deferred. Not commitments. Each needs a design pass before
it's built. The bar stays high: the volume side stays a simple executor.

## Persistent app-settings store (opt-in)

**Idea.** A small, veracage-owned encrypted store (separate from the user's
data volumes) that holds a sandboxed app's config so settings survive across
sessions. Today the app's `$HOME`/XDG dirs are an ephemeral tmpfs (see
`sandbox.py`), so the volume stays pristine (only what the user explicitly
saves) but every session starts factory-default.

**Why it's not just "add a mount".** App state is a *side channel* into the
user's other volumes: recent-files lists, session files, search history, and
thumbnail caches reference or contain fragments of the encrypted documents.
Persisting them moves those traces out of the volume into a longer-lived
place, and a single shared store leaks volume-A activity into a volume-B
session. So "ephemeral by default" is a privacy property, not just a
simplification.

**If built, the constraints that fall out:**
- **Encrypt it.** A plain host dir would put document-name traces unencrypted
  on disk, worse than ephemeral.
- **Key it without friction.** Its own passphrase = an extra prompt. Better
  to unlock via the login keyring / session.
- **Scope it.** Shared-across-all-volumes leaks cross-volume. Per-volume
  avoids that but is more machinery, and it can't live *inside* the volume
  (no hidden files there, by design), so it'd be a separate paired store per
  volume.
- **Settings-only.** Persist `katerc`/keybindings/theme, exclude
  history/cache/sessions. Captures the UX win, drops most of the leak, but
  it's per-app curation.
- **Opt-in**, ephemeral remaining the default.
- Runs as the veracage uid, so it inherits the same-uid residual (readable by
  a same-uid attacker) - fine for settings, another reason to keep document
  history out of it.

**Recommended v1 (when the ephemeral default proves painful):** opt-in,
encrypted, settings-only, keyring-unlocked. Until then, keep it ephemeral.
