"""bwrap argv shape — the design's security flags must always be present."""
from __future__ import annotations

import os
from pathlib import Path

import pytest

from veracage import sandbox
from veracage.apps import App

_KATE = App("kate", "Kate", "kate", ["/vault"])
_OKULAR = App("okular", "Okular", "okular")


@pytest.fixture
def argv():
    sock = Path("/tmp/veracage-test.sock")
    return sandbox.bwrap_command("/run/veracage/abc", _KATE, sock)


def _adjacent(argv, flag, value):
    """True if `flag value` appears consecutively in argv."""
    return any(argv[i] == flag and argv[i + 1] == value
               for i in range(len(argv) - 1))


def test_exchange_bound_at_slash_exchange_when_given():
    sock = Path("/tmp/veracage-test.sock")
    argv = sandbox.bwrap_command("/run/veracage/abc", _KATE, sock,
                                 exchange="/run/veracage/abc.x")
    # bound at a TOP-LEVEL /exchange, never under /vault (encrypted-at-rest invariant)
    assert _adjacent(argv, "/run/veracage/abc.x", "/exchange")
    assert "/vault/exchange" not in argv


def test_no_exchange_bind_by_default(argv):
    assert "/exchange" not in argv


REQUIRED_FLAGS = [
    # namespace flags — required for isolation
    "--unshare-pid",
    "--unshare-uts",
    "--unshare-ipc",
    "--unshare-cgroup-try",
    "--unshare-net",
    # safety
    "--die-with-parent",
    "--new-session",
]


@pytest.mark.parametrize("flag", REQUIRED_FLAGS)
def test_required_flag_present(argv, flag):
    assert flag in argv, f"{flag} missing from bwrap argv"


def test_starts_with_bwrap(argv):
    assert argv[0] == "bwrap"


def test_app_command_appears_after_double_dash(argv):
    sep = argv.index("--")
    assert argv[sep + 1] == "kate"
    assert "/vault" in argv[sep + 1:]


def test_vault_is_bound_at_slash_vault(argv):
    # `--bind <mp> /vault` must be present
    pairs = list(zip(argv, argv[1:], argv[2:]))
    assert any(
        a == "--bind" and b == "/run/veracage/abc" and c == "/vault"
        for a, b, c in pairs
    ), "vault not bound at /vault"


def test_runtime_dir_is_tmpfs(argv):
    """The host's $XDG_RUNTIME_DIR must NOT be bound through wholesale —
    instead a tmpfs hides it, and only the wayland socket is bound."""
    uid = os.getuid()
    rt = f"/run/user/{uid}"
    pairs = list(zip(argv, argv[1:]))

    assert ("--tmpfs", rt) in pairs, "runtime dir is not a tmpfs"

    # ...and it must be private (0700): Qt refuses a group/world-readable
    # XDG_RUNTIME_DIR, and the nested wayland socket lives inside it.
    ti = next(i for i, (a, b) in enumerate(pairs) if a == "--tmpfs" and b == rt)
    assert argv[ti - 2:ti] == ["--perms", "0700"], "runtime tmpfs is not 0700"

    # Forbid any --bind / --ro-bind of the entire runtime dir
    forbidden = [
        i for i, (a, b) in enumerate(pairs)
        if a in {"--bind", "--ro-bind"} and b == rt
    ]
    assert not forbidden, "host runtime dir is wholesale-bound — clipboard leak risk"


def test_only_wayland_socket_bound_into_runtime_dir(argv):
    """The Weston socket should be bound at <runtime>/wayland-0; nothing else."""
    uid = os.getuid()
    target = f"/run/user/{uid}/wayland-0"
    triples = list(zip(argv, argv[1:], argv[2:]))
    binds_into_runtime = [
        (b, c) for a, b, c in triples
        if a == "--bind" and c.startswith(f"/run/user/{uid}/")
    ]
    assert binds_into_runtime == [("/tmp/veracage-test.sock", target)]


def test_wayland_display_is_wayland_0(argv):
    triples = list(zip(argv, argv[1:], argv[2:]))
    assert ("--setenv", "WAYLAND_DISPLAY", "wayland-0") in triples


def test_home_is_vault_xdg_is_ephemeral_off_vault(argv):
    """HOME is the vault so open/save dialogs default to the user's documents,
    while XDG config/cache/data live on an ephemeral tmpfs OUTSIDE the vault —
    so nothing app-generated (not even an empty dotdir) is written into it."""
    triples = list(zip(argv, argv[1:], argv[2:]))
    pairs = list(zip(argv, argv[1:]))
    assert ("--setenv", "HOME", "/vault") in triples
    # XDG dirs are on the ephemeral /xdg tmpfs, never inside the vault.
    assert ("--tmpfs", "/xdg") in pairs, "/xdg is not an ephemeral tmpfs"
    assert ("--setenv", "XDG_CONFIG_HOME", "/xdg/config") in triples
    assert ("--setenv", "XDG_CACHE_HOME", "/xdg/cache") in triples
    assert ("--setenv", "XDG_DATA_HOME", "/xdg/data") in triples
    # No XDG_* points into the vault.
    assert not any(
        k.startswith("XDG_") and str(v).startswith("/vault")
        for a, k, v in triples if a == "--setenv"
    ), "an XDG dir points into the vault"


def test_no_share_user_no_share_net_no_network(argv):
    """Belt-and-suspenders: no --share-net and no --share-user flags."""
    assert "--share-net" not in argv
    assert "--share-user" not in argv


def test_app_args_are_passed(argv):
    # Kate's catalog entry is ["/vault"]; the bwrap argv ends with `kate /vault`.
    assert argv[-2:] == ["kate", "/vault"]


def test_chdir_to_vault(argv):
    assert ("--chdir", "/vault") in list(zip(argv, argv[1:]))


def test_etc_is_not_wholesale_bound(argv):
    """Security: the whole host /etc must not be exposed — only curated paths."""
    pairs = list(zip(argv, argv[1:]))
    assert ("--ro-bind", "/etc") not in pairs
    assert ("--bind", "/etc") not in pairs


def test_essential_etc_paths_bound(argv):
    """Rendering deps from /etc must survive the curation: fontconfig, the
    dynamic linker, NSS for getpwuid, timezone, machine-id for Qt/D-Bus."""
    pairs = list(zip(argv, argv[1:]))
    bound = {b for a, b in pairs if a in {"--ro-bind", "--ro-bind-try"}}
    for needed in ("/etc/fonts", "/etc/ld.so.cache", "/etc/passwd",
                   "/etc/group", "/etc/nsswitch.conf", "/etc/localtime",
                   "/etc/machine-id"):
        assert needed in bound, f"{needed} not bound into sandbox"


def test_gpu_off_by_default_no_dri(argv):
    assert "/dev/dri" not in argv


def test_gpu_opt_in_binds_dev_dri():
    sock = Path("/tmp/veracage-test.sock")
    argv = sandbox.bwrap_command("/run/veracage/abc", _OKULAR,
                                 sock, gpu=True)
    triples = list(zip(argv, argv[1:], argv[2:]))
    assert ("--dev-bind-try", "/dev/dri", "/dev/dri") in triples
    # Must come after `--dev /dev` so it binds into the fresh devtmpfs.
    assert argv.index("/dev/dri") > argv.index("/dev")


def test_no_places_file_by_default(argv):
    assert "/xdg/data/user-places.xbel" not in argv


def test_places_fd_written_as_writable_file_after_xdg_tmpfs():
    sock = Path("/tmp/veracage-test.sock")
    argv = sandbox.bwrap_command("/run/veracage/abc", _KATE,
                                 sock, places_fd=7)
    triples = list(zip(argv, argv[1:], argv[2:]))
    # --file (not --ro-bind): a WRITABLE tmpfs file, so Dolphin can rewrite it
    # (it merges its default places on startup) instead of erroring "not writable".
    assert ("--file", "7", "/xdg/data/user-places.xbel") in triples
    assert argv.index("/xdg/data/user-places.xbel") > argv.index("/xdg")
