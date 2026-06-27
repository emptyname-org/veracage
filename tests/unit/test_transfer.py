"""File transfer staging dirs and import/export."""
from __future__ import annotations

import pytest

from veracage import transfer


def test_ensure_staging_creates_dirs(tmp_path):
    in_dir, out_dir = transfer.ensure_staging(tmp_path)
    assert in_dir.is_dir()
    assert out_dir.is_dir()
    assert in_dir == tmp_path / ".veracage" / "in"
    assert out_dir == tmp_path / ".veracage" / "out"


def test_ensure_staging_idempotent(tmp_path):
    transfer.ensure_staging(tmp_path)
    transfer.ensure_staging(tmp_path)  # must not raise
    assert (tmp_path / ".veracage" / "in").is_dir()


def test_staging_dir_perms_0o700(tmp_path):
    in_dir, out_dir = transfer.ensure_staging(tmp_path)
    assert (in_dir.stat().st_mode & 0o777) == 0o700
    assert (out_dir.stat().st_mode & 0o777) == 0o700


def test_import_copies_file(tmp_path):
    src = tmp_path / "src" / "doc.txt"
    src.parent.mkdir()
    src.write_text("hello")

    vault = tmp_path / "vault"
    vault.mkdir()

    dst = transfer.import_file(src, vault)
    assert dst.read_text() == "hello"
    assert dst.parent == vault / ".veracage" / "in"
    assert (dst.stat().st_mode & 0o777) == 0o600
    # original is untouched (copy, not move)
    assert src.read_text() == "hello"


def test_import_missing_source_raises(tmp_path):
    with pytest.raises(FileNotFoundError):
        transfer.import_file(tmp_path / "no-such-file", tmp_path)


def test_import_collision_renames(tmp_path):
    src = tmp_path / "doc.txt"
    src.write_text("v1")
    vault = tmp_path / "vault"
    vault.mkdir()

    a = transfer.import_file(src, vault)
    b = transfer.import_file(src, vault)
    c = transfer.import_file(src, vault)
    assert a.name == "doc.txt"
    assert b.name == "doc (1).txt"
    assert c.name == "doc (2).txt"


def test_export_moves_file(tmp_path):
    vault = tmp_path / "vault"
    _, out_dir = transfer.ensure_staging(vault)
    src = out_dir / "out.txt"
    src.write_text("export me")

    target = tmp_path / "host" / "saved.txt"
    target.parent.mkdir()
    transfer.export_file(src, target)
    assert target.read_text() == "export me"
    assert not src.exists()


def test_list_outbox_sorted(tmp_path):
    _, out = transfer.ensure_staging(tmp_path)
    (out / "b.txt").write_text("")
    (out / "a.txt").write_text("")
    (out / "c.txt").write_text("")
    files = transfer.list_outbox(tmp_path)
    assert [p.name for p in files] == ["a.txt", "b.txt", "c.txt"]


def test_list_outbox_skips_subdirs(tmp_path):
    _, out = transfer.ensure_staging(tmp_path)
    (out / "f.txt").write_text("")
    (out / "subdir").mkdir()
    files = transfer.list_outbox(tmp_path)
    assert [p.name for p in files] == ["f.txt"]


def test_unique_path_no_collision(tmp_path):
    p = tmp_path / "x.txt"
    assert transfer.unique_path(p) == p


def test_unique_path_increments(tmp_path):
    (tmp_path / "x.txt").write_text("")
    p1 = transfer.unique_path(tmp_path / "x.txt")
    assert p1.name == "x (1).txt"
    p1.write_text("")
    p2 = transfer.unique_path(tmp_path / "x.txt")
    assert p2.name == "x (2).txt"
