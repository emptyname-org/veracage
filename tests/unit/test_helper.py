"""Privilege-boundary regression tests for helpers/veracage-helper.

The helper runs as root via pkexec and is reachable by any active local
user (polkit auth_self_keep). It must derive the target uid/gid from
PKEXEC_UID — never from caller-supplied argv — and pin the continuation
to its own install tree. Otherwise a direct

    pkexec veracage-helper --user 0 --continuation /bin/sh ...

would yield a root shell (local privilege escalation).
"""
from __future__ import annotations

import importlib.util
import inspect
import os
import pwd
from importlib.machinery import SourceFileLoader
from pathlib import Path

import pytest

HELPER = Path(__file__).resolve().parents[2] / "helpers" / "veracage-helper"


def _load_helper():
    # The helper has no .py extension, so an explicit source loader is needed.
    loader = SourceFileLoader("veracage_helper", str(HELPER))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


helper = _load_helper()


def test_resolve_caller_uses_pkexec_uid():
    uid = os.getuid()
    if uid == 0:
        pytest.skip("test runner is root; PKEXEC_UID=0 is rejected by design")
    expected_gid = pwd.getpwuid(uid).pw_gid
    assert helper.resolve_caller({"PKEXEC_UID": str(uid)}) == (uid, expected_gid)


def test_resolve_caller_only_reads_environ():
    """No argv/uid parameter through which a caller could choose identity."""
    assert list(inspect.signature(helper.resolve_caller).parameters) == ["environ"]


def test_resolve_caller_rejects_missing_pkexec_uid():
    with pytest.raises(PermissionError):
        helper.resolve_caller({})


def test_resolve_caller_rejects_non_numeric_pkexec_uid():
    with pytest.raises(PermissionError):
        helper.resolve_caller({"PKEXEC_UID": "root"})


def test_resolve_caller_rejects_root_caller():
    with pytest.raises(PermissionError):
        helper.resolve_caller({"PKEXEC_UID": "0"})


def test_continuation_is_pinned_to_repo_tree():
    cont = helper.continuation_path()
    assert cont == HELPER.resolve().parent.parent / "src" / "bin" / "veracage"
    assert cont.is_file(), f"pinned continuation {cont} missing on disk"


def test_helper_argv_contract_has_no_identity_flags():
    """Regression: the helper must not accept --user/--group/--continuation."""
    src = HELPER.read_text()
    assert 'add_argument("--user"' not in src
    assert 'add_argument("--group"' not in src
    assert 'add_argument("--continuation"' not in src
    assert "PKEXEC_UID" in src  # identity derived the safe way
