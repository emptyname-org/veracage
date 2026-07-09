"""The `App` type.

There is no fixed catalog / whitelist any more: the user enables *any* installed
binary (`veracage configure --add <binary>`), and it is launched in the sandbox.
`exec` is the binary (looked up on $PATH at add time); `args` are appended on
launch (typically `/vault` so the app opens at the vault root).
"""
from __future__ import annotations

from dataclasses import dataclass, field


@dataclass(frozen=True)
class App:
    key: str
    name: str
    exec: str
    args: list[str] = field(default_factory=list)
