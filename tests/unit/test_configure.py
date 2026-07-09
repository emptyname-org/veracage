"""Configure subcommand: --add / --remove / --list (any installed binary, no catalog)."""
from __future__ import annotations

import argparse

from veracage import config, configure


def _ns(**kw):
    fields = {"add": None, "remove": None, "list": False,
              "name": None, "arg": None, "key": None}
    fields.update(kw)
    return argparse.Namespace(**fields)


def test_add_enables_installed_binary(tmp_xdg_config, fake_path_with):
    fake_path_with(["kate"])
    assert configure._add(_ns(add="kate", arg=["/vault"])) == 0
    cfg = config.load()
    assert cfg.apps["kate"].exec == "kate"
    assert cfg.apps["kate"].args == ["/vault"]


def test_add_rejects_uninstalled(tmp_xdg_config, fake_path_with):
    fake_path_with([])  # empty $PATH
    assert configure._add(_ns(add="nope")) == 2
    assert config.load().is_empty()


def test_add_any_binary_not_a_catalog(tmp_xdg_config, fake_path_with):
    """Any installed binary can be enabled — there is no whitelist to be in."""
    fake_path_with(["my-weird-tool"])
    assert configure._add(_ns(add="my-weird-tool", name="Weird")) == 0
    assert config.load().apps["my-weird-tool"].name == "Weird"


def test_add_absolute_path(tmp_xdg_config, tmp_path):
    binary = tmp_path / "tool"
    binary.write_text("#!/bin/sh\n")
    binary.chmod(0o755)
    assert configure._add(_ns(add=str(binary))) == 0
    assert config.load().apps["tool"].exec == str(binary)


def test_readd_updates_not_duplicates(tmp_xdg_config, fake_path_with):
    fake_path_with(["kate"])
    configure._add(_ns(add="kate", arg=["/vault"]))
    configure._add(_ns(add="kate", name="Kate 2"))
    cfg = config.load()
    assert len(cfg.apps) == 1
    assert cfg.apps["kate"].name == "Kate 2"


def test_remove_disables(tmp_xdg_config, fake_path_with):
    fake_path_with(["kate"])
    configure._add(_ns(add="kate"))
    assert configure._remove(_ns(remove="kate")) == 0
    assert config.load().is_empty()


def test_remove_unknown_is_error(tmp_xdg_config):
    assert configure._remove(_ns(remove="ghost")) == 2


def test_list_shows_enabled(tmp_xdg_config, fake_path_with, capsys):
    fake_path_with(["kate"])
    configure._add(_ns(add="kate"))
    assert configure._list(_ns(list=True)) == 0
    assert "kate" in capsys.readouterr().out
