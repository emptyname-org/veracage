"""The App type (there is no catalog / whitelist any more)."""
from __future__ import annotations

import dataclasses

import pytest

from veracage import apps


def test_app_has_expected_fields():
    a = apps.App(key="kate", name="Kate", exec="kate", args=["/vault"])
    assert (a.key, a.name, a.exec, a.args) == ("kate", "Kate", "kate", ["/vault"])


def test_app_args_default_empty():
    assert apps.App(key="x", name="X", exec="x").args == []


def test_app_is_frozen():
    a = apps.App(key="kate", name="Kate", exec="kate")
    with pytest.raises(dataclasses.FrozenInstanceError):
        a.name = "Notkate"  # type: ignore[misc]


def test_no_catalog_symbols():
    """The hardcoded whitelist is gone — nothing should reintroduce it silently."""
    for gone in ("KNOWN_APPS", "detected", "CATEGORY_LABELS", "CATEGORY_ORDER"):
        assert not hasattr(apps, gone)
