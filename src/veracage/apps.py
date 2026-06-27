"""Catalog of well-known apps and detection of which ones are installed."""
from __future__ import annotations

import shutil
from dataclasses import dataclass, field


@dataclass(frozen=True)
class App:
    key: str
    name: str
    category: str
    exec: str
    args: list[str] = field(default_factory=list)
    note: str = ""


# Curated catalog. `exec` is the binary name we look up via shutil.which().
# `args` are appended after the binary on launch (typically `/vault` so the
# app opens at the vault root).
KNOWN_APPS: dict[str, App] = {a.key: a for a in [
    # Text editors
    App("kate",       "Kate",                 "text",  "kate",       ["/vault"]),
    App("kwrite",     "KWrite",               "text",  "kwrite",     []),
    App("gedit",      "gedit",                "text",  "gedit",      []),
    App("mousepad",   "Mousepad",             "text",  "mousepad",   []),
    App("featherpad", "FeatherPad",           "text",  "featherpad", []),
    App("gnome-text", "GNOME Text Editor",    "text",  "gnome-text-editor", []),

    # PDF viewers
    App("okular",     "Okular",               "pdf",   "okular",     []),
    App("evince",     "Evince",               "pdf",   "evince",     []),
    App("qpdfview",   "qpdfview",             "pdf",   "qpdfview",   []),
    App("zathura",    "Zathura",              "pdf",   "zathura",    []),
    App("mupdf",      "MuPDF",                "pdf",   "mupdf",      []),
    App("xpdf",       "Xpdf",                 "pdf",   "xpdf",       []),

    # File managers
    App("dolphin",    "Dolphin",              "files", "dolphin",    ["/vault"]),
    App("nautilus",   "Files (Nautilus)",     "files", "nautilus",   ["/vault"]),
    App("thunar",     "Thunar",               "files", "thunar",     ["/vault"]),
    App("pcmanfm-qt", "PCManFM-Qt",           "files", "pcmanfm-qt", ["/vault"]),
    App("nemo",       "Nemo",                 "files", "nemo",       ["/vault"]),

    # Image viewers
    App("gwenview",   "Gwenview",             "image", "gwenview",   ["/vault"]),
    App("loupe",      "Loupe",                "image", "loupe",      []),
    App("eog",        "Eye of GNOME",         "image", "eog",        []),
    App("feh",        "feh",                  "image", "feh",        ["/vault"]),

    # Office (heavy — opt-in)
    App("lowriter",   "LibreOffice Writer",   "office","lowriter",   [],
        note="Pulls in heavy LibreOffice deps; sandbox start is slow."),
    App("localc",     "LibreOffice Calc",     "office","localc",     []),
]}

CATEGORY_ORDER = ["text", "pdf", "files", "image", "office"]
CATEGORY_LABELS = {
    "text":   "Text editors",
    "pdf":    "PDF viewers",
    "files":  "File managers",
    "image":  "Image viewers",
    "office": "Office",
}


def detected() -> dict[str, App]:
    """Return only the catalog entries whose exec is on $PATH."""
    return {k: a for k, a in KNOWN_APPS.items() if shutil.which(a.exec)}
