# Future features — think-about-later list

Ideas deliberately deferred. Not commitments; each needs a design pass before
it's built. Keep the bar high: the vault side stays a simple executor.

## Before the first GitHub push (sequenced)

1. **UI fixes** — polish the single-window UX on the real box; add the
   empty-compositor front door (see `single-window-ux.md` §Deferred), which lets us
   delete the startup picker.
2. **Code-cleanup / deletion pass** — after the UI fixes: cut what the redesign made
   obsolete (dead paths superseded by the compositor/broker/exchange). Favour net
   deletion; propose the removals as a list before cutting.
3. **Naming review** — the vocabulary is still provisional (`glossary.md`), incl. the
   product name; settle it before publishing.

Then push.

## Persistent app-settings store (opt-in)

**Idea.** A small, veracage-owned encrypted store — separate from the user's
data vaults — that holds a sandboxed app's config so settings survive across
sessions. Today the app's `$HOME`/XDG dirs are an ephemeral tmpfs (see
`sandbox.py`), so the vault stays pristine (only what the user explicitly
saves) but every session starts factory-default.

**Why it's not just "add a mount".** App state is a *side channel* into the
user's other vaults: recent-files lists, session files, search history, and
thumbnail caches reference or contain fragments of the encrypted documents.
Persisting them moves those traces out of the vault into a longer-lived place,
and a single shared store leaks vault-A activity into a vault-B session. So
"ephemeral by default" is a privacy property, not just a simplification.

**If built, the constraints that fall out:**
- **Encrypt it.** A plain host dir would put document-name traces unencrypted
  on disk — worse than ephemeral.
- **Key it without friction.** Its own passphrase = an extra prompt; better to
  unlock via the login keyring / session.
- **Scope it.** Shared-across-all-vaults leaks cross-vault; per-vault avoids
  that but is more machinery — and it can't live *inside* the vault (no hidden
  files there, by design), so it'd be a separate paired store per vault.
- **Settings-only.** Persist `katerc`/keybindings/theme; exclude
  history/cache/sessions. Captures the UX win, drops most of the leak — but
  it's per-app curation.
- **Opt-in**, ephemeral remaining the default.
- Runs as the vault uid, so it inherits the same-uid residual (readable by a
  same-uid attacker) — fine for settings, another reason to keep document
  history out of it.

**Recommended v1 (when the ephemeral default proves painful):** opt-in,
encrypted, settings-only, keyring-unlocked. Until then, keep it ephemeral.
