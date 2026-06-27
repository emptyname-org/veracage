"""Catalog and detection."""
from __future__ import annotations

from veracage import apps


def test_catalog_keys_match_App_key():
    for k, a in apps.KNOWN_APPS.items():
        assert k == a.key, f"catalog key {k!r} != App.key {a.key!r}"


def test_catalog_categories_known():
    for a in apps.KNOWN_APPS.values():
        assert a.category in apps.CATEGORY_LABELS, (
            f"{a.key} has unknown category {a.category!r}"
        )


def test_catalog_keys_unique():
    keys = list(apps.KNOWN_APPS)
    assert len(keys) == len(set(keys))


def test_detected_returns_only_present(fake_path_with):
    fake_path_with(["kate", "okular"])
    found = apps.detected()
    assert set(found) == {"kate", "okular"}


def test_detected_empty_when_path_empty(fake_path_with):
    fake_path_with([])
    assert apps.detected() == {}


def test_detected_returns_App_instances(fake_path_with):
    fake_path_with(["dolphin"])
    found = apps.detected()
    assert isinstance(found["dolphin"], apps.App)
    assert found["dolphin"].category == "files"
    assert "/vault" in found["dolphin"].args


def test_app_dataclass_is_frozen():
    a = apps.KNOWN_APPS["kate"]
    import dataclasses
    with __import__("pytest").raises(dataclasses.FrozenInstanceError):
        a.name = "Notkate"  # type: ignore[misc]
