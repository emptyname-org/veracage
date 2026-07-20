"""The App type (there is no catalog / whitelist any more)."""
from __future__ import annotations

import dataclasses

import pytest

from veracage import apps


def test_app_has_expected_fields():
    a = apps.App(key="kate", name="Kate", exec="kate")
    assert (a.key, a.name, a.exec) == ("kate", "Kate", "kate")


def test_app_has_no_args_field():
    """The launch-dir args feature was removed; apps always launch bare."""
    assert not hasattr(apps.App(key="x", name="X", exec="x"), "args")


def test_app_is_frozen():
    a = apps.App(key="kate", name="Kate", exec="kate")
    with pytest.raises(dataclasses.FrozenInstanceError):
        a.name = "Notkate"  # type: ignore[misc]


def test_no_catalog_symbols():
    """The hardcoded whitelist is gone. Nothing should reintroduce it silently."""
    for gone in ("KNOWN_APPS", "detected", "CATEGORY_LABELS", "CATEGORY_ORDER"):
        assert not hasattr(apps, gone)
