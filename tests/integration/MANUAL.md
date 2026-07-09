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

The nested compositor and clipboard bridge are our own Rust binaries
(`veracage-compositor` / `veracage-agent`), built by `make install` — no
`weston` or `wl-clipboard` needed.

Create a 50 MB test vault (password: `veracage-test`):

```
make test-vault         # follow printed instructions
```

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
# Inside the sandbox app, open a "Run command" or terminal-emulator that
# the catalog includes (or temporarily edit apps.py to add `bash`):
src/bin/veracage open /tmp/veracage-test.vc <terminal-app>

# In the sandbox:
$ ip a                              # expect: only "lo"
$ getent hosts example.com          # expect: failure
$ python3 -c 'import socket; socket.create_connection(("1.1.1.1",80),2)'
# expect: OSError [Errno 101] Network is unreachable
```

## Test 3 — Wayland clipboard isolation (req 1.1.2)

```bash
# Inside the sandbox kate:
1. Type "secret-text-from-vault" into the editor
2. Select all → Ctrl+C
3. Switch to a host text editor (kate or kwrite) outside the sandbox
4. Ctrl+V

Expected: paste yields whatever was on the host clipboard before;
          "secret-text-from-vault" is NOT pasted.
```

Repeat in reverse: copy in host, paste in sandbox — should also fail.

✅ Pass if the two clipboards are independent.

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
# Inside kate: New file → write "hello" → save as /vault/note.txt → quit.

src/bin/veracage open /tmp/veracage-test.vc kate
# Inside kate: open /vault/note.txt → expect "hello".
```

## Test 6 — Cleanup

After closing the sandbox app:

```bash
ls /dev/mapper/ | grep veracage    # expect: NO output
ls /run/veracage/                  # expect: empty (or dir absent)
mount | grep veracage              # expect: NO output
```

## Slice 3a — multi-app session

### Test 7 — Add an app to a running session

```bash
src/bin/veracage open  /tmp/veracage-test.vc kate          # terminal A
src/bin/veracage exec  /tmp/veracage-test.vc okular        # terminal B
src/bin/veracage list  /tmp/veracage-test.vc
src/bin/veracage close /tmp/veracage-test.vc
```

✅ Pass if Okular appears in the same Weston window as Kate, no second
password prompt, `list` shows two apps, and `close` cleanly terminates
both + dismounts.

### Test 8 — Clipboard between sandbox apps

In Kate (sandbox), copy text. Paste into Dolphin's address bar (also
sandbox). Should work. Paste into a host editor — should fail.

## Slice 3b — agent + bridges

### Test 9 — Tray icon present

After `veracage open`, a tray icon appears on the host panel with the
"security-high" theme icon. Hover shows the vault filename.

### Test 10 — Push host clipboard → sandbox

```
1. Copy "host-text-001" to host clipboard from any host app.
2. Right-click the Veracage tray icon → "Push host clipboard → sandbox"
3. In a sandbox app (Kate), Ctrl+V.
```

✅ Pass if "host-text-001" appears in Kate.

### Test 11 — Pull sandbox clipboard → host

```
1. In Kate (sandbox), copy "vault-text-002".
2. Tray → "Pull sandbox clipboard → host"
3. In any host app, Ctrl+V.
```

✅ Pass if "vault-text-002" appears in the host app.

### Test 12 — Drop zone import

```
1. Tray → "Show drop zone…" → small window appears on the host.
2. Drag a host file onto it.
3. In sandbox Dolphin, navigate to /vault/.veracage/in/
```

✅ Pass if the file is visible in the inbox with size + mtime preserved
(`copy2`), permissions `0600`, and the original on the host is unchanged.

### Test 13 — Outbox export

```
1. In sandbox Kate: write a file → save to /vault/.veracage/out/note.txt.
2. Tray balloon notification appears within ~1s ("1 file(s) in outbox").
3. Left-click the tray icon → file dialog asks where to save.
4. Pick a host path, accept.
```

✅ Pass if the file moves to the host path and `out/note.txt` no longer
exists.

## Slice 4 — crash-safe cleanup + suspend

### Test 14 — SIGKILL the launcher, dm-crypt is still cleaned up

```bash
# Terminal A
src/bin/veracage open /tmp/veracage-test.vc kate

# Terminal B — find the launcher PID
pgrep -a -f 'veracage open'

# Terminal B — kill the *entire unit*, simulating a crash
loginctl kill-session $(loginctl | awk '$3=="'$USER'"{print $1; exit}') --signal=SIGKILL
# OR more targeted:
systemctl --user stop veracage-*.service

# Terminal B — verify
ls /dev/mapper/ | grep veracage    # expect: NO output
ls /run/veracage/*.lock 2>/dev/null # expect: NO output
mount | grep veracage              # expect: NO output
```

✅ Pass if no veracage state leaks after the kill. (Without Slice 4, the
dm-crypt device would remain.)

### Test 15 — Suspend dismounts the vault

```bash
# Open a vault, then suspend the laptop:
src/bin/veracage open /tmp/veracage-test.vc kate
systemctl suspend
# resume
ls /dev/mapper/ | grep veracage    # expect: NO output
```

✅ Pass if waking up requires re-entering the vault password.

### Test 16 — Klipper still excluded after agent transfer

After Test 11, open Klipper history. The pulled-to-host text is now on
the host clipboard — that's correct (the user explicitly pulled it).
Confirm that text the sandbox has **not** had pulled is **absent** from
Klipper. In other words: clipboard isolation is per-explicit-action,
not all-or-nothing.
