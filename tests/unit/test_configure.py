"""Configure subcommand: --auto, --list, --text."""
from __future__ import annotations

import argparse

from veracage import config, configure


def _ns(**kw):
    fields = {"auto": False, "list": False, "text": False}
    fields.update(kw)
    return argparse.Namespace(**fields)


def test_auto_writes_detected(tmp_xdg_config, fake_path_with):
    fake_path_with(["kate", "okular", "dolphin"])
    rc = configure._auto(_ns(auto=True))
    assert rc == 0
    cfg = config.load()
    assert set(cfg.apps) == {"kate", "okular", "dolphin"}


def test_auto_overwrites_existing(tmp_xdg_config, fake_path_with):
    fake_path_with(["kate"])
    configure._auto(_ns(auto=True))
    fake_path_with(["okular"])
    configure._auto(_ns(auto=True))
    cfg = config.load()
    assert set(cfg.apps) == {"okular"}


def test_list_prints_marks(tmp_xdg_config, fake_path_with, capsys):
    fake_path_with(["kate"])
    # save kate as enabled
    configure._auto(_ns(auto=True))
    rc = configure._list(_ns(list=True))
    out = capsys.readouterr().out
    assert rc == 0
    assert "[x] kate" in out
    assert "[-] gedit" in out  # not installed → "[-]"


def test_text_yes_enables(tmp_xdg_config, fake_path_with, monkeypatch, capsys):
    fake_path_with(["kate", "okular"])
    answers = iter(["y", "n"])
    monkeypatch.setattr("builtins.input", lambda *_: next(answers))
    configure._text(_ns(text=True))
    cfg = config.load()
    assert "kate" in cfg.apps
    assert "okular" not in cfg.apps


def test_text_empty_keeps_current(tmp_xdg_config, fake_path_with, monkeypatch):
    fake_path_with(["kate"])
    # bootstrap with kate enabled
    configure._auto(_ns(auto=True))
    # then run text mode and just press Enter
    monkeypatch.setattr("builtins.input", lambda *_: "")
    configure._text(_ns(text=True))
    cfg = config.load()
    assert "kate" in cfg.apps
