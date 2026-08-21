# Writing style

Apply these rules to all prose, documentation, comments, UI text, and generated messages.

## Vocabulary

- Use consistent terminology throughout the project.
- Before introducing a new term, check whether an equivalent term is already used.
- Do not use multiple words for the same concept.
- Follow the canonical vocabulary listed below.

## Length: alerts, dialogs and notifications

- Keep every one of them to one short line.
- Say what to do, not why. Three or four words is usually enough:
  "Close Dolphin to continue."
- Do not explain the mechanism, restate the obvious, or add reassurance.
- Longer prose belongs in Help, not in an alert.
- **Never buy brevity with precision.** Cut padding, never the exact word.
  "Kate failed to launch" is both shorter and more precise than "Kate did not
  start": the first says the launch was attempted and failed, the second could
  mean anything. Name what happened, not a vaguer category of it.
- Say which thing and what state it is in, when that is what the reader needs:
  "<volume> was not mounted: its filesystem needs a repair", not "check <volume>".
- Write examples with a placeholder (`<volume>`, `<app>`) rather than a made-up
  name. A bare "work" reads as a word, not as the volume it stands for.

## Technical text: documentation, comments, Help

The length rule above does NOT apply here. These are read by someone who wants
to know how something works, so:

- Precision first. Name the real mechanism, file, syscall or exit code.
- Say what was measured and where, not what is assumed.
- Same vocabulary as the UI, so a doc and a menu name the same thing alike.
- No marketing tone, no reassurance, no restating the code in prose.

## Punctuation

- Do not use semicolons in prose.
- Do not use en dashes or em dashes.
- Use commas, periods, parentheses, or colons instead.
- Use straight quotation marks: " and '.
- Do not use curved or smart quotation marks.
- Do not alter punctuation that is required by programming-language syntax.

## Canonical vocabulary

- Use "Veracage", not "vault", "composer", or "sandbox".
- The host os should be called Host, not desktop
- Use "Volume", not "disk".
- Volumes are opened and closed. Never "mounted", "dismounted" or "unmounted"
  in text the user reads: "open" and "close" are the pair, the same one the
  authentication prompt uses ("open or close the volume"). A volume's state is
  open, not mounted. Internal names keep "mount"/"dismount"/"close-volume", and
  technical text keeps "mount" where it names the mechanism (idmap-mount, mount
  namespace, /proc/mounts).
- Use "Shared directory" for the directory both the Host and Veracage see, not "exchange folder" or "exchange directory". Internal names (the exchange config key, /exchange, ~/Veracage/Exchange) stay.
- Clipboard actions move the clipboard (what was copied), not the "selection".
- Use "directory", not "folder"
