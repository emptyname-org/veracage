# Manual & integration tests

Unit tests (`pytest`) cover argv shape, config round-trip, and detection
logic. The tests in this directory exercise things unit tests can't:
real privilege drop, real mount NS, real Wayland.

## Automated checks

`test_helper_security.py` asserts the privilege-boundary argument contract on
the **built** Rust helper (no vault/Wayland/root needed — every case fails
before any mount). Run after `make build`:

```
pytest tests/integration/test_helper_security.py -v
```

The tests below are manual: they need a real Wayland session, a vault, and
the polkit policy installed (`make install-dev`).

## Prerequisites

```
sudo apt install python3-pytest bubblewrap cryptsetup veracrypt
make install-dev    # polkit policy
```

The nested compositor and the human-side agent are our own Rust binaries
(`veracage-compositor` / `veracage-agent`), built by `make install` — no
`weston` or `wl-clipboard` needed.

Create a 50 MB test vault (password: `vvv`):

```
make test-vault
```

Enable the apps the tests launch (no fixed catalog — any installed binary):

```
src/bin/veracage configure --add kate --add okular --add dolphin
```

The test vault has no filesystem label, so it mounts in the sandbox at
`/vaults/veracage-test` (the source's file stem). Substitute your label below.

## Test 1 — Mount NS isolation (req 1.1.1)

**What it proves:** the decrypted volume is invisible to host processes.

```bash
# Terminal A: open the vault
src/bin/veracage open /tmp/veracage-test.vc kate

# Terminal B (separate): scan host mount table
mount | grep veracage     # expect: NO output
ls /run/veracage/         # expect: directory entries owned by root, but
                          #         their contents inaccessible / empty
findmnt --no-truncate | grep -E 'veracage|tcrypt'   # expect: NO output
```

✅ Pass if Terminal B sees no veracage mountpoint.
❌ Fail if any host-side `mount` / `findmnt` shows the decrypted volume.

## Test 2 — Network blocked (`--unshare-net`)

```bash
# Enable a terminal-emulator app, then open the vault with it:
src/bin/veracage configure --add konsole
src/bin/veracage open /tmp/veracage-test.vc konsole

# In the sandbox:
$ ip a                              # expect: only "lo"
$ getent hosts example.com          # expect: failure
$ python3 -c 'import socket; socket.create_connection(("1.1.1.1",80),2)'
# expect: OSError [Errno 101] Network is unreachable
```

## Test 3 — Wayland clipboard isolation (req 1.1.2)

The compositor owns a **separate** clipboard; a plain Ctrl+C in the sandbox
does not reach the host clipboard (crossing is user-triggered only —
Ctrl+Alt+C sandbox→host / Ctrl+Alt+V host→sandbox, or the toolbar buttons).

```bash
# Inside the sandbox kate:
1. Type "secret-text-from-vault" into the editor
2. Select all → Ctrl+C
3. Switch to a host text editor (kate or kwrite) outside the sandbox
4. Ctrl+V

Expected: paste yields whatever was on the host clipboard before;
          "secret-text-from-vault" is NOT pasted.
```

Repeat in reverse: copy in host, paste in sandbox — should also fail without
the explicit Ctrl+Alt+V.

✅ Pass if the two clipboards are independent until an explicit transfer.

## Test 4 — Klipper does not scrape the sandbox

KDE Klipper (clipboard manager) keeps a history.

```bash
# Open the sandbox kate, copy "trigger-string-vault-001"
# Open Klipper from the host taskbar / Win+V
```

Expected: `trigger-string-vault-001` does NOT appear in Klipper's history.

✅ Pass if Klipper history is uncontaminated.

## Test 5 — Persistence

```bash
src/bin/veracage open /tmp/veracage-test.vc kate
# Inside kate: New file → write "hello" → save as /vaults/veracage-test/note.txt → quit.

src/bin/veracage open /tmp/veracage-test.vc kate
# Inside kate: open /vaults/veracage-test/note.txt → expect "hello".
```

## Test 6 — Cleanup

After closing the vault (`veracage close /tmp/veracage-test.vc`, or File → Close
vault in the toolbar):

```bash
ls /dev/mapper/ | grep veracage    # expect: NO output
ls /run/veracage/                  # expect: no session-* state
mount | grep veracage              # expect: NO output
```

## Test 7 — Multi-app session

```bash
src/bin/veracage open /tmp/veracage-test.vc kate
# In the compositor's Apps menu, launch a second app (Okular).
src/bin/veracage list  /tmp/veracage-test.vc      # expect: two apps
src/bin/veracage close /tmp/veracage-test.vc
```

✅ Pass if Okular appears in the same compositor window as Kate, no second
password prompt, `list` shows two apps, and `close` cleanly terminates both +
dismounts.

## Test 8 — Clipboard between sandbox apps

In Kate (sandbox), copy text. Paste into Dolphin's address bar (also sandbox) —
should work (both connect to the same compositor clipboard). Paste into a host
editor — should fail.

## Test 9 — Exchange folder (host ↔ vault file transfer)

The shared folder replaces the old socket file bridge: `~/Veracage/Exchange` on
the host is idmap-mounted into the sandbox at `/exchange`.

```bash
# Host: drop a file in
echo hi > ~/Veracage/Exchange/from_host.txt
# Sandbox (Dolphin): navigate to /exchange → from_host.txt is there, owned by you.
# Sandbox: save a file to /exchange/from_sandbox.txt
# Host: ~/Veracage/Exchange/from_sandbox.txt appears, owned by you.
```

✅ Pass if files cross both ways, owned by the human uid, with no dialogs.

## Crash-safe cleanup + suspend

### Test 10 — SIGKILL the session, dm-crypt is still cleaned up

```bash
# Terminal A
src/bin/veracage open /tmp/veracage-test.vc kate

# Terminal B — kill the whole session unit, simulating a crash
systemctl --user stop 'veracage-*.service'

# Terminal B — verify (ExecStopPost cleanup ran)
ls /dev/mapper/ | grep veracage         # expect: NO output
ls /run/veracage/session-*.lock 2>/dev/null   # expect: NO output
mount | grep veracage                   # expect: NO output
```

✅ Pass if no veracage state leaks after the kill.

### Test 11 — Suspend dismounts the vault

```bash
# Open a vault, then suspend the laptop:
src/bin/veracage open /tmp/veracage-test.vc kate
systemctl suspend
# resume
ls /dev/mapper/ | grep veracage    # expect: NO output
```

✅ Pass if waking up requires re-entering the vault password.
