# Manual & integration tests

Unit tests (`pytest`) cover argv shape, config round-trip, and detection
logic. The tests in this directory exercise things unit tests can't:
real privilege drop, real mount NS, real Wayland.

## Automated checks

`test_helper_security.py` asserts the privilege-boundary argument contract on
the **built** Rust helper (no volume/Wayland/root needed: every case fails
before any mount). Run after `make build`:

```
pytest tests/integration/test_helper_security.py -v
```

The tests below are manual: they need a real Wayland session, a volume, and
the polkit policy installed (`make install-dev`).

## Prerequisites

```
sudo apt install python3-pytest bubblewrap cryptsetup veracrypt
make build build-agent build-compositor
make install-dev    # polkit policy, udev rule, sleep hook (runs sudo install)
```

The nested compositor and the human-side agent are our own Rust binaries
(`veracage-compositor` / `veracage-agent`), which the CLI picks up straight
from the checkout once they are built. No
`weston` or `wl-clipboard` needed.

Create a 50 MB test volume (password: `vvv`):

```
make test-vault
```

Enable the apps the tests launch (no fixed catalog - any installed binary),
in the Apps > Configure apps... dialog, or directly:

```
agent-rs/target/release/veracage-agent configure
```

The test volume has no filesystem label, so it mounts in the sandbox at
`/vaults/veracage-test` (the source's file stem). Substitute your label below.

## Test 1: Mount NS isolation

**What it proves:** the decrypted volume is invisible to host processes.

```bash
# Terminal A: open the volume
src/bin/veracage open /tmp/veracage-test.vc kate

# Terminal B (separate): scan host mount table
mount | grep veracage     # expect: NO output
ls /run/veracage/         # expect: directory entries owned by root, but
                          #         their contents inaccessible / empty
findmnt --no-truncate | grep -E 'veracage|tcrypt'   # expect: NO output
```

Pass if Terminal B sees no veracage mountpoint.
Fail if any host-side `mount` / `findmnt` shows the decrypted volume.

## Test 2: Network blocked (`--unshare-net`)

```bash
# Enable a terminal-emulator app in Apps > Configure apps..., then open the
# volume with it:
src/bin/veracage open /tmp/veracage-test.vc konsole

# In the sandbox:
$ ip a                              # expect: only "lo"
$ getent hosts example.com          # expect: failure
$ python3 -c 'import socket; socket.create_connection(("1.1.1.1",80),2)'
# expect: OSError [Errno 101] Network is unreachable
```

## Test 3: Wayland clipboard isolation

The compositor owns a **separate** clipboard. A plain Ctrl+C in the sandbox
does not reach the host clipboard (crossing is user-triggered only:
Ctrl+Alt+C sandbox->host / Ctrl+Alt+V host->sandbox, or the Clipboard >
Copy out / Clipboard > Paste in menu items).

```bash
# Inside the sandbox kate:
1. Type "secret-text-from-volume" into the editor
2. Select all → Ctrl+C
3. Switch to a host text editor (kate or kwrite) outside the sandbox
4. Ctrl+V

Expected: paste yields whatever was on the host clipboard before.
          "secret-text-from-volume" is NOT pasted.
```

Repeat in reverse: copy in host, paste in sandbox - should also fail without
the explicit Ctrl+Alt+V.

Pass if the two clipboards are independent until an explicit transfer.

## Test 4: Klipper does not scrape the sandbox

KDE Klipper (clipboard manager) keeps a history.

```bash
# Open the sandbox kate, copy "trigger-string-volume-001"
# Open Klipper from the host taskbar / Win+V
```

Expected: `trigger-string-volume-001` does NOT appear in Klipper's history.

Pass if Klipper history is uncontaminated.

## Test 5: Persistence

```bash
src/bin/veracage open /tmp/veracage-test.vc kate
# Inside kate: New file → write "hello" → save as /vaults/veracage-test/note.txt → quit.

src/bin/veracage open /tmp/veracage-test.vc kate
# Inside kate: open /vaults/veracage-test/note.txt → expect "hello".
```

## Test 6: Cleanup

After closing the volume (`veracage close /tmp/veracage-test.vc`, or File >
Quit in the menu):

```bash
ls /dev/mapper/ | grep veracage    # expect: NO output
ls /run/veracage/                  # expect: no session-* state
mount | grep veracage              # expect: NO output
```

## Test 7: Multi-app session

```bash
src/bin/veracage open /tmp/veracage-test.vc kate
# In the compositor's Apps menu, launch a second app (Okular).
src/bin/veracage list  /tmp/veracage-test.vc      # expect: two apps
src/bin/veracage close /tmp/veracage-test.vc
```

Pass if Okular appears in the same compositor window as Kate, no second
password prompt, `list` shows two apps, and `close` cleanly terminates both
and closes the volume.

## Test 8: Clipboard between sandbox apps

In Kate (sandbox), copy text. Paste into Dolphin's address bar (also sandbox) -
should work (both connect to the same compositor clipboard). Paste into a host
editor - should fail.

## Test 9: Shared directory (host <-> volume file transfer)

`~/Veracage/Exchange` on the host is idmap-mounted into the sandbox at
`/exchange`.

```bash
# Host: drop a file in
echo hi > ~/Veracage/Exchange/from_host.txt
# Sandbox (Dolphin): navigate to /exchange → from_host.txt is there, owned by you.
# Sandbox: save a file to /exchange/from_sandbox.txt
# Host: ~/Veracage/Exchange/from_sandbox.txt appears, owned by you.
```

Pass if files cross both ways, owned by the human uid, with no dialogs.

## Crash-safe cleanup + suspend

### Test 10: SIGKILL the session, dm-crypt is still cleaned up

```bash
# Terminal A
src/bin/veracage open /tmp/veracage-test.vc kate

# Terminal B - kill the whole session unit, simulating a crash
systemctl --user stop 'veracage-*.service'

# Terminal B - verify (ExecStopPost cleanup ran)
ls /dev/mapper/ | grep veracage         # expect: NO output
ls /run/veracage/session-*.lock 2>/dev/null   # expect: NO output
mount | grep veracage                   # expect: NO output
```

Pass if no veracage state leaks after the kill.

### Test 11: Suspend closes the volume

```bash
# Open a volume, then suspend the laptop:
src/bin/veracage open /tmp/veracage-test.vc kate
systemctl suspend
# resume
ls /dev/mapper/ | grep veracage    # expect: NO output
```

Pass if waking up requires re-entering the volume passphrase.

# What no human has run yet

Everything below arrived with the 0.6.0 quit/close redesign and the fixes on
top of it. All of it passes the gate, and the privileged parts are proven by
the VPS spikes, but none of it has been seen on a screen. Run the quit and
close tests first: a failure there is the one that can leave a volume
decrypted.

## Quit and close

### Test 12: Quit with a clean app

```bash
src/bin/veracage open /tmp/veracage-test.vc kate
# File > Quit
ls /dev/mapper/ | grep veracage    # expect: NO output
```

Expect, in this order: Kate's window closes, the volume closes, then the
Veracage window goes away. Fail if the window goes away while a dm device is
still there: the exit is gated on the session lock being gone, not on a timer.

### Test 13: Quit with unsaved work (no override, no timeout)

Type into Kate without saving, then File > Quit.

- Kate's own "save changes?" prompt comes up inside the Veracage window.
- The Veracage window stays. Cancel Kate's prompt: Veracage stays open and
  stays usable. Leave it five minutes to prove nothing times out.
- File > Quit again: Kate is asked again, never overridden.
- Answer Discard: the quit finishes as in test 12.

Fail if Veracage disappears, if Kate is killed under its own prompt, or if a
second Quit forces the exit.

### Test 14: Quit with nothing open

```bash
src/bin/veracage _up      # empty session, no volume
# File > Quit
```

Expect: exits at once.

### Test 15: Quit while a volume is being opened

Press File > Quit while the passphrase prompt or the "Unlocking <volume>" note
is still up. Expect: it does not exit while the volume is open or half open.
The session lock is written before the mount, so this window is covered.

### Test 16: File > Close volume with an app running

This is the one that used to report success without closing anything.

```bash
src/bin/veracage open /tmp/veracage-test.vc kate
# File > Close volume > veracage-test
```

Expect "Close Kate to continue." with Close and Cancel.

- Cancel: the volume stays open and stays usable (open a file from it in Kate).
- Close: Kate closes, then the volume really closes.

```bash
ls /dev/mapper/ | grep veracage                # expect: NO output
src/bin/veracage list /tmp/veracage-test.vc    # expect: not in the session
```

### Test 17: Close one volume of two

Open two volumes, close one from File > Close volume. Expect: the other stays
open and usable, the title says "1 volume open (<label>)", and exactly one dm
device is gone.

### Test 18: File > Close volume > All

Every volume closes, the window stays up showing "No volume open" and the hint
"File > Open volume".

## Idle

### Test 19: The idle close asks once, not once per retry

Settings > System Integration > Auto-close after: 1 minute, then leave the
machine alone. Expect one "Closing after 1 minutes idle" notice and, if an app
holds the volume, ONE "Close <app> to continue." question rather than a new one
every retry gap. Move the mouse before the minute is up: the timer restarts.

## The menu to agent channel

### Test 20: Menu items in quick succession

Without waiting between clicks: File > Open volume... (cancel the picker),
File > Shared directory, Apps > Configure apps... (close it), Help > About
Veracage. Expect each to act exactly once. The single-slot mailbox this
replaced could lose one command to the next within a poll.

### Test 21: One broker only

With a session running, run `src/bin/veracage open /tmp/veracage-test.vc`
again from a terminal.

```bash
pgrep -c -x veracage-agent    # expect: 1
```

Expect one passphrase prompt, not two, and no doubled menu actions afterwards.

### Test 22: The question neither blocks the loop nor outlives its helper

With "Close Kate to continue." on screen:

- The menu bar still opens and the notice area still updates. It used to block
  the whole broker until the human answered.
- Leave the question up and close the volume another way:
  `src/bin/veracage close-volume /tmp/veracage-test.vc`. Expect the question to
  come down by itself.

## Helper

### Test 23: A volume file that is not yours

The helper runs as root, so it now checks that the account that authenticated
could open the source itself. Two sudo lines to set it up:

```bash
sudo chown root:root /tmp/veracage-test.vc
```

Then `src/bin/veracage open /tmp/veracage-test.vc kate` expects a refusal
saying the volume "is not yours to open", before any passphrase prompt.
Restore with:

```bash
sudo chown $USER:$USER /tmp/veracage-test.vc
```

### Test 24: The same volume twice, by two paths

With it open, open it again through another path to the same file (a symlink,
or `/tmp/../tmp/veracage-test.vc`).

Expect "is already open in the running workspace", and no second dm device:
the check is on the source's device and inode, not on the path string.

```bash
ls /dev/mapper/ | grep -c veracage    # expect: unchanged
```

## Config and startup

### Test 25: One bad value does not erase the settings

```bash
cp ~/.config/veracage/config.toml /tmp/config.bak
sed -i 's|^exchange_dir .*|exchange_dir = 5|' ~/.config/veracage/config.toml
src/bin/veracage _up
```

Expect: it starts, prints "invalid exchange_dir 5, using the default", and
every other setting (apps, theme, auto-close, clipboard) is still there.
Restore with `cp /tmp/config.bak ~/.config/veracage/config.toml`.

### Test 26: A font file that is not a font

```bash
VERACAGE_FONT_FILE=/etc/hostname src/bin/veracage _up
```

Expect: starts with the default font. It used to panic inside epaint's parser.

## Vocabulary

### Test 27: Walk the UI after the rename

File > Open volume..., File > Close volume, the backdrop "No volume open" with
"File > Open volume", the title "2 volumes open (a, b)", Settings > Auto-close
after and On system suspend > Close (recommended) / Leave open, and Help.

The passphrase dialog must still say Unlock, and its note must still read
"Unlocking <volume>": that names the key derivation, not the open. Anything
still saying Mount or Dismount to the human is a miss.

## Clipboard and suspend

### Test 28: The host clipboard clears itself

Settings > Clear host clipboard: After 30 seconds. Copy text in a sandbox app,
Clipboard > Copy out (Ctrl+Alt+C), paste on the host: it arrives. Wait past the
timeout and paste again: nothing. Then, with something on the host clipboard
from a Copy out, close the last volume: it is cleared at close.

### Test 29: Suspend, both settings

With a volume open and On system suspend > Close (recommended):

```bash
systemctl suspend
# resume
ls /dev/mapper/ | grep veracage    # expect: NO output
```

Repeat with Leave open: the volume is still open on resume. Test 11 covers the
basic case, this one covers the hook's rewritten grace period and the setting
it now reads without following a symlink.

## Input

### Test 30: A modifier held when the host takes the keyboard

With an app open in Veracage, hold Ctrl down and, while still holding it, make
the host take the keyboard away: a host global shortcut that switches desktop
(Ctrl+Alt+Right on KDE), or Alt+Tab. Let go of Ctrl outside the Veracage
window, click back into it, and roll the mouse wheel in the app.

Expect: the wheel scrolls. Before the compositor released held keys on focus
loss it zoomed instead, letters ran shortcuts, and only a restart cleared it,
because the release for that Ctrl was delivered to the host and never to the
nested seat. With `debug = true`, the log line for a quiet second reads
`mods=none`, and a latched modifier reads `mods=Ctrl`.

## Install

### Test 31: One password prompt per start, none at quit

```bash
command -v veracage                                       # which install runs
pkaction --action-id org.veracage.helper --verbose | grep exec.path
```

Expect: the annotated path is the helper of THAT install
(`<prefix>/libexec/veracage/veracage-helper`). polkit keys an action to the
pkexec'd program path and the host has one policy file, so a second install
under another prefix (a .deb over a `make install`) silently takes it over.

Then start Veracage and quit it.

Expect: ONE password prompt (the compositor's, which `auth_self_keep` reuses
for the session moments later), and NO prompt at all when quitting - the
teardown runs the cleanup action, which is passwordless by policy. A prompt per
step, or a prompt at quit, means pkexec matched no action and fell back to
`org.freedesktop.policykit.exec`. `veracage open` says so on stderr when it
sees the mismatch.

### Test 32: The debug logs are where the configuration says

With `debug = true` and `log_dir = "<some dir the veracage uid can write>"`:

```bash
ls "$LOG_DIR"                       # expect: compositor.log and leader.log
journalctl _UID=$(id -u veracage) -f   # expect: the sandboxed apps' own output
```

Expect: `compositor.log` gets a line a second, `leader.log` a line per app
launch and exit. Point `log_dir` at a path that is not a directory: `veracage`
says so on stderr and logs to `/run/veracage/rt/compositor.log` instead. Plant
a symlink at `<log_dir>/compositor.log`: it is refused (O_NOFOLLOW), not
followed.

### Test 33: Apps follow the Host's theme, whatever it is

Settings > Appearance > Theme: Follow the Host. On the Host, apply a colour
scheme that is not Breeze (System Settings > Colours), a non-Breeze widget style
and a system-wide icon theme, then launch an app inside Veracage.

Expect: the app comes up in the Host's colours, with that widget style and those
icons, and the Veracage window follows the same light or dark. Check the seed
if it does not:

```bash
grep -A3 '\[Colors:Window\]' /run/veracage/pub/apptheme   # the Host's own groups
head -2 /run/veracage/pub/apptheme                        # icon theme, widget style
cat /run/veracage/pub/theme                               # resolved: light or dark
```

Then switch the Host to an icon theme installed only under `~/.local/share/icons`
and open a volume again. Expect: Breeze icons, not missing ones, and a line on
stderr saying the theme is not installed system-wide. The sandbox has no host
home, so it could not load that theme.

Then set Theme: Light. Expect: Breeze light apps regardless of the Host, and
`/run/veracage/pub/apptheme` empty.
