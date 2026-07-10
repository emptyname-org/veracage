"""The `App` type.

There is no fixed catalog / whitelist any more: the user enables *any* installed
binary (`veracage configure --add <binary>`), and it is launched in the sandbox.
`exec` is the binary (looked up on $PATH at add time); `args` are appended on
launch (typically `/vault` so the app opens at the vault root).
"""
from __future__ import annotations

import os
from dataclasses import dataclass, field

# Known file-manager binaries — matched on the `exec` basename. Used to: auto-launch
# one when a vault opens with no app named (cli), nudge the user to enable one (the
# picker), and tell the compositor which app "opens the vault" (the desktop tile).
# Not a catalog, just a hint set.
FILE_MANAGERS = frozenset({
    "dolphin", "nautilus", "nemo", "thunar", "pcmanfm", "pcmanfm-qt",
    "caja", "konqueror", "krusader", "nnn", "ranger",
})


@dataclass(frozen=True)
class App:
    key: str
    name: str
    exec: str
    args: list[str] = field(default_factory=list)


def is_file_manager(exec_: str) -> bool:
    """True if `exec_`'s basename is a known file manager."""
    return os.path.basename(exec_) in FILE_MANAGERS
