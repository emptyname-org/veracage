"""bwrap argv shape: the design's security flags must always be present."""
from __future__ import annotations

import os
from pathlib import Path

import pytest

from veracage import sandbox
from veracage.apps import App

_KATE = App("kate", "Kate", "kate")
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
    # bound at a TOP-LEVEL /exchange, never under /vaults (encrypted-at-rest invariant)
    assert _adjacent(argv, "/run/veracage/abc.x", "/exchange")
    assert "/vaults/exchange" not in argv


def test_no_exchange_bind_by_default(argv):
    assert "/exchange" not in argv


REQUIRED_FLAGS = [
    # namespace flags: required for isolation
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
    # The app is what bwrap ultimately runs: last, after the `--` separator and
    # the private-bus wrapper (see test_app_runs_under_a_private_dbus_session).
    sep = argv.index("--")
    assert argv[sep + 1:] == [*sandbox.dbus_wrapper(), "kate"]


def test_vault_is_bound_at_slash_vault(argv):
    # `--bind <mp> /vaults` must be present
    pairs = list(zip(argv, argv[1:], argv[2:]))
    assert any(
        a == "--bind" and b == "/run/veracage/abc" and c == "/vaults"
        for a, b, c in pairs
    ), "vault not bound at /vaults"


def test_runtime_dir_is_tmpfs(argv):
    """The host's $XDG_RUNTIME_DIR must NOT be bound through wholesale.
    Instead a tmpfs hides it, and only the wayland socket is bound."""
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
    assert not forbidden, "host runtime dir is wholesale-bound: clipboard leak risk"


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
    while XDG config/cache/data live on an ephemeral tmpfs OUTSIDE the vault,
    so nothing app-generated (not even an empty dotdir) is written into it."""
    triples = list(zip(argv, argv[1:], argv[2:]))
    pairs = list(zip(argv, argv[1:]))
    assert ("--setenv", "HOME", "/vaults") in triples
    # XDG dirs are on the ephemeral /xdg tmpfs, never inside the vault.
    assert ("--tmpfs", "/xdg") in pairs, "/xdg is not an ephemeral tmpfs"
    assert ("--setenv", "XDG_CONFIG_HOME", "/xdg/config") in triples
    assert ("--setenv", "XDG_CACHE_HOME", "/xdg/cache") in triples
    assert ("--setenv", "XDG_DATA_HOME", "/xdg/data") in triples
    # No XDG_* points into the vault.
    assert not any(
        k.startswith("XDG_") and str(v).startswith("/vaults")
        for a, k, v in triples if a == "--setenv"
    ), "an XDG dir points into the vault"


def test_qt_platform_theme_is_set_so_the_seeded_kdeglobals_applies(argv):
    """The seeded kdeglobals only reaches a Qt app through the KDE platform
    theme. Without this the app keeps the generic theme (fusion style, its own
    font, light palette) and the configured theme and font are ignored."""
    triples = list(zip(argv, argv[1:], argv[2:]))
    assert ("--setenv", "QT_QPA_PLATFORMTHEME", "kde") in triples


def test_no_share_user_no_share_net_no_network(argv):
    """Belt-and-suspenders: no --share-net and no --share-user flags."""
    assert "--share-net" not in argv
    assert "--share-user" not in argv


def test_app_launches_bare(argv):
    # Apps launch with no arguments (the launch-dir args feature was removed);
    # the sandbox chdir (/vaults) is what places them in the workspace. The only
    # thing allowed between bwrap's `--` and the app is the private-bus wrapper.
    assert argv[-1] == "kate"
    sep = argv.index("--")
    assert argv[sep + 1:] == [*sandbox.dbus_wrapper(), "kate"]


def test_app_runs_under_a_private_dbus_session():
    """Qt/KDE apps block on the D-Bus connect timeout (measured: 25s of an app
    doing nothing, then "Not connected to D-Bus server") when there is no session
    bus. Each app gets its own, started inside the sandbox - never the host's."""
    if not sandbox.dbus_wrapper():
        pytest.skip("dbus-run-session is not installed")
    sock = Path("/run/user/1000/wayland-0")
    argv = sandbox.bwrap_command("/run/veracage/abc", _KATE, sock)
    sep = argv.index("--")
    assert argv[sep + 1:sep + 3] == ["dbus-run-session", "--"]
    # No host bus address or socket is passed in: the bus lives in the sandbox.
    assert not any("DBUS_SESSION_BUS_ADDRESS" in a for a in argv)
    assert not any("/run/user/1000/bus" in a for a in argv)


def test_bwrap_runs_exec_as_single_argv():
    """Security invariant (set-apps confused-deputy containment): the app `exec`
    is passed as ONE argv element after `--`, never split into a command +
    args and never through a shell. So even if a same-uid `set-apps` swaps the
    launch list, a swapped entry can only name a bare program (a nonexistent
    multi-token string just fails to exec). It cannot inject arguments."""
    sock = Path("/run/user/1000/wayland-0")
    weird = App(key="x", name="X", exec="kate --evil; rm -rf ~")
    argv = sandbox.bwrap_command("/run/veracage/abc", weird, sock)
    sep = argv.index("--")
    # The exec is the LAST element and still exactly one element, so it can only
    # name a program (this one fails to exec). `dbus-run-session` is in front of
    # it on hosts that have it, and execs its command directly - no shell there
    # either - so the containment argument is unchanged.
    assert argv[sep + 1:] == [*sandbox.dbus_wrapper(), "kate --evil; rm -rf ~"]


def test_chdir_to_vault(argv):
    assert ("--chdir", "/vaults") in list(zip(argv, argv[1:]))


def test_cursor_theme_forwarded_from_the_session(monkeypatch):
    """The pointer must match the host session. Without XCURSOR_SIZE an app picks
    the theme's next size up, which showed as visibly larger cursors inside
    Veracage; the theme name and size are forwarded when the session sets them."""
    monkeypatch.setenv("XCURSOR_THEME", "breeze_cursors")
    monkeypatch.setenv("XCURSOR_SIZE", "24")
    argv = sandbox.bwrap_command("/run/veracage/abc", _KATE, Path("/tmp/s.sock"))
    triples = list(zip(argv, argv[1:], argv[2:]))
    assert ("--setenv", "XCURSOR_THEME", "breeze_cursors") in triples
    assert ("--setenv", "XCURSOR_SIZE", "24") in triples


def test_cursor_theme_absent_when_the_session_has_none(monkeypatch):
    monkeypatch.delenv("XCURSOR_THEME", raising=False)
    monkeypatch.delenv("XCURSOR_SIZE", raising=False)
    argv = sandbox.bwrap_command("/run/veracage/abc", _KATE, Path("/tmp/s.sock"))
    assert "XCURSOR_THEME" not in argv
    assert "XCURSOR_SIZE" not in argv


# Every bwrap flag that can bring a host path INTO the sandbox. A test that
# checks only `--ro-bind` misses the `-try` variants, which is what the real
# argv uses everywhere.
_BIND_FLAGS = ("--bind", "--bind-try", "--ro-bind", "--ro-bind-try",
               "--dev-bind", "--dev-bind-try")


def _binds(argv):
    """[(flag, source, dest)] for every bind in argv."""
    out = []
    for i, a in enumerate(argv):
        if a in _BIND_FLAGS and i + 2 < len(argv):
            out.append((a, argv[i + 1], argv[i + 2]))
    return out


def test_etc_is_not_wholesale_bound(argv):
    """Security: the whole host /etc must not be exposed, only curated paths.
    Checks EVERY bind flavour: the real argv binds with the `-try` variants, so
    a test that named only `--ro-bind` would not have noticed `--ro-bind-try
    /etc /etc` being added."""
    assert not [b for b in _binds(argv) if b[1] == "/etc" or b[2] == "/etc"]


def test_no_host_filesystem_beyond_the_curated_set_is_bound_in(argv):
    """The sandbox sees /usr, a curated /etc, the font cache, the GPU nodes, the
    workspace and the Wayland socket. Anything else bound in from the host is a
    hole in "no host filesystem", so this asserts the WHOLE set rather than
    spot-checking absences."""
    allowed_prefixes = (
        "/usr", "/etc/", "/var/cache/fontconfig", "/dev/dri",
        "/sys/dev/char", "/sys/devices",
        "/run/veracage/",          # the workspace itself
        "/tmp/veracage-test.sock",  # the nested Wayland socket in this fixture
    )
    for flag, src, dest in _binds(argv):
        assert src.startswith(allowed_prefixes), \
            f"{flag} {src} -> {dest} brings an uncurated host path in"


def test_usr_is_read_only(argv):
    """/usr is the app's own code and must not be writable from the sandbox."""
    usr = [b for b in _binds(argv) if b[1] == "/usr"]
    assert usr, "/usr must be bound"
    assert all(flag.startswith("--ro-bind") for flag, _, _ in usr), usr


def test_environment_is_cleared_before_anything_is_set(argv):
    """--clearenv is what stops the app inheriting the leader's environment
    (host DISPLAY, session tokens, auth socket paths). Without it every --setenv
    below it is additive to whatever the leader happened to hold."""
    assert "--clearenv" in argv
    # And it must come before the --setenv flags it exists to bound.
    assert argv.index("--clearenv") < argv.index("--setenv")


def test_a_stray_host_variable_does_not_reach_the_sandbox(monkeypatch):
    """The env is an allowlist, not inheritance: a variable the leader happens to
    carry (a token, an auth socket) must not appear in the sandbox argv."""
    monkeypatch.setenv("VERACAGE_TEST_SECRET", "swordfish")
    argv = sandbox.bwrap_command("/run/veracage/abc", _KATE, Path("/tmp/s.sock"))
    assert "VERACAGE_TEST_SECRET" not in argv
    assert "swordfish" not in argv


def test_essential_etc_paths_bound(argv):
    """Rendering deps from /etc must survive the curation: fontconfig, the
    dynamic linker, NSS for getpwuid, timezone, machine-id for Qt/D-Bus, and both
    hops of the cursor-theme symlink chain (alternatives -> X11/cursors), without
    which apps show no resize cursors."""
    pairs = list(zip(argv, argv[1:]))
    bound = {b for a, b in pairs if a in {"--ro-bind", "--ro-bind-try"}}
    for needed in ("/etc/fonts", "/etc/ld.so.cache", "/etc/passwd",
                   "/etc/group", "/etc/nsswitch.conf", "/etc/localtime",
                   "/etc/machine-id", "/etc/alternatives", "/etc/X11/cursors"):
        assert needed in bound, f"{needed} not bound into sandbox"


def test_gpu_always_bound(argv):
    """The GPU is always passed through: the render node plus the /sys device
    metadata Mesa needs to identify the hardware (without the /sys binds it
    falls back to software rendering)."""
    triples = list(zip(argv, argv[1:], argv[2:]))
    assert ("--dev-bind-try", "/dev/dri", "/dev/dri") in triples
    assert ("--ro-bind-try", "/sys/dev/char", "/sys/dev/char") in triples
    assert ("--ro-bind-try", "/sys/devices", "/sys/devices") in triples
    # Must come after `--dev /dev` so it binds into the fresh devtmpfs.
    assert argv.index("/dev/dri") > argv.index("/dev")


def test_no_seed_files_by_default(argv):
    assert "/xdg/data/user-places.xbel" not in argv
    assert "/xdg/config/mimeapps.list" not in argv


def test_seeds_written_as_writable_files_after_xdg_tmpfs():
    sock = Path("/tmp/veracage-test.sock")
    argv = sandbox.bwrap_command("/run/veracage/abc", _KATE, sock,
                                 seeds=[(7, "/xdg/data/user-places.xbel"),
                                        (8, "/xdg/config/mimeapps.list")])
    triples = list(zip(argv, argv[1:], argv[2:]))
    # --file (not --ro-bind): a WRITABLE tmpfs file, so Dolphin can rewrite it
    # (it merges its default places on startup) instead of erroring "not writable".
    assert ("--file", "7", "/xdg/data/user-places.xbel") in triples
    assert ("--file", "8", "/xdg/config/mimeapps.list") in triples
    assert argv.index("/xdg/data/user-places.xbel") > argv.index("/xdg")
