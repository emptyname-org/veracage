"""CLI entry: argparse + subcommand dispatch."""
from __future__ import annotations

import argparse
import contextlib
import json
import os
import secrets
import shutil
import subprocess
import sys
import time
from pathlib import Path

from . import cleanup, config, configure, leader
from .apps import FILE_MANAGERS
from .sandbox import bwrap_command  # noqa: F401  (kept for downstream tests)

# Privileged helpers. An install (`make install`) sets VERACAGE_HELPER /
# VERACAGE_CLEANUP_HELPER in the generated launcher to point at $LIBEXEC.
# For a source checkout we derive paths from the repo root. Whatever this
# resolves to MUST match the polkit policy's exec.path.
_REPO_ROOT = Path(__file__).resolve().parents[2]


def _default_mount_helper() -> str:
    """The built Rust helper (make install-dev). There is deliberately no
    fallback: the removed Python reference helper dropped to the caller uid
    with no idmap, so falling back silently would bypass the whole isolation
    model. A missing binary fails loudly at pkexec time instead."""
    return str(_REPO_ROOT / "helper-rs" / "target" / "release" / "veracage-helper")


HELPER_PATH = os.environ.get("VERACAGE_HELPER", _default_mount_helper())
CLEANUP_HELPER_PATH = os.environ.get(
    "VERACAGE_CLEANUP_HELPER",
    str(_REPO_ROOT / "helpers" / "veracage-cleanup"),
)


def _default_agent() -> str:
    """The human-side GUI/CLI agent (Rust). An install sets VERACAGE_AGENT to
    the installed binary; for a source checkout, prefer the built one."""
    for prof in ("release", "debug"):
        p = _REPO_ROOT / "agent-rs" / "target" / prof / "veracage-agent"
        if p.is_file():
            return str(p)
    return "veracage-agent"  # installed: on $PATH


AGENT_PATH = os.environ.get("VERACAGE_AGENT", _default_agent())


def _default_compositor() -> str:
    """The nested compositor (Rust/smithay). An install sets VERACAGE_COMPOSITOR
    to the installed binary; for a source checkout, prefer the built one, else
    fall back to the name on $PATH."""
    for prof in ("release", "debug"):
        p = _REPO_ROOT / "compositor-rs" / "target" / prof / "veracage-compositor"
        if p.is_file():
            return str(p)
    return "veracage-compositor"  # installed: on $PATH


COMPOSITOR_PATH = os.environ.get("VERACAGE_COMPOSITOR", _default_compositor())

# Env vars to forward through pkexec (which strips the environment).
# WAYLAND_DISPLAY + XDG_RUNTIME_DIR are required for the nested compositor to
# reach the host compositor and place its socket; the rest are useful for
# theming / locale.
_FORWARD_ENV = (
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "DISPLAY",
    "LANG",
    "VERACAGE_THEME",        # compositor egui theme (light default; "dark" opts in)
    "VERACAGE_FONT_FILE",    # compositor UI font FILE (resolved from config ui_font)
    "VERACAGE_FONT_SIZE",    # compositor UI base point size (config ui_font_size)
    "VERACAGE_WINDOW_SIZE",  # compositor default window size (config window_size)
    "VERACAGE_DEBUG",        # verbose timing logs (config debug); see docs/debugging.md
)


def _forward_env_args() -> list[str]:
    """`--setenv K=V` pairs for the allowlisted env pkexec forwards into the
    helper (WAYLAND_DISPLAY + XDG_RUNTIME_DIR are required to reach the host
    compositor / place sockets; the rest are theming/locale)."""
    out: list[str] = []
    for k in _FORWARD_ENV:
        v = os.environ.get(k)
        if v is not None:
            out += ["--setenv", f"{k}={v}"]
    return out


def _exec_stop_post(sid: str) -> str:
    """The transient unit's ExecStopPost: pkexec the cleanup helper for this
    session so every dm is closed on any exit (SIGKILL/OOM/panic/logout)."""
    return f'ExecStopPost=pkexec "{CLEANUP_HELPER_PATH}" --session {sid}'


def _exchange_flags(cfg: config.Config) -> list[str]:
    """`--exchange <dir>` for the helper when the shared directory is enabled and
    can be prepared, else []. Creates the dir and tightens perms to 0700 ONLY
    when we just created it (a pre-existing shared dir keeps its perms); the
    helper re-validates ownership before idmap-mounting it."""
    if not cfg.exchange:
        return []
    xdir = cfg.exchange_path()
    try:
        existed = xdir.exists()
        xdir.mkdir(parents=True, exist_ok=True)
        if not existed:
            xdir.chmod(0o700)
        return ["--exchange", str(xdir)]
    except OSError as e:
        print(f"veracage: could not prepare the shared directory ({e}). "
              "Continuing without it.", file=sys.stderr)
        return []


# ------------------------------------------------- persistent compositor --

def cmd_compositor(args: argparse.Namespace) -> int:
    """Hidden: exec the compositor binary. Invoked (as `veracage _compositor`)
    only by the helper after it has dropped to the veracage uid and set
    WAYLAND_SOCKET / XDG_RUNTIME_DIR, so the surviving process is the persistent
    compositor. The helper brings it up as part of the vault-mount pkexec."""
    exe = COMPOSITOR_PATH
    resolved = exe if Path(exe).is_file() else shutil.which(exe)
    if resolved is None:
        print(f"veracage: compositor binary not found: {exe}", file=sys.stderr)
        return 1
    os.execv(resolved, [resolved, "--socket", args.socket])


# Once the compositor has AUTHENTICATED (its pidfile appears, written by the
# helper right before it execs the compositor), its EGL/Wayland init is machine-
# paced and sub-second, so bound only THAT with a short timeout. The pre-auth
# wait (the polkit password prompt) is unbounded human time and must NOT be
# clock-bounded at all; it's gated on the unit staying alive instead.
COMPOSITOR_INIT_TIMEOUT_S = 15.0

# ActiveStates that mean the unit is done (and, since we haven't seen a socket,
# done UNSUCCESSFULLY: auth cancelled/denied, or the compositor exited).
_UNIT_DEAD_STATES = ("failed", "inactive", "deactivating")


def _unit_active_state(unit: str) -> str:
    """systemd --user ActiveState of `unit` (active/activating/failed/inactive/…),
    or '' if it can't be queried (treated as 'keep waiting', not failure)."""
    try:
        r = subprocess.run(
            ["systemctl", "--user", "show", "-p", "ActiveState", "--value", unit],
            capture_output=True, text=True)
        return r.stdout.strip()
    except OSError:
        return ""


def ensure_compositor_up() -> int:
    """Bring up the ONE persistent compositor as its OWN systemd --user transient
    unit (so it outlives any single vault session: a compositor forked inside a
    session's unit is killed when that unit's cgroup is torn down). Idempotent:
    a no-op if it's already running. Blocks until its socket appears. Returns 0
    on success, non-zero on failure. Shared by `veracage _up` and `cmd_open`."""
    from . import wayland
    if wayland.compositor_is_up():
        return 0
    # Forward the compositor's look (theme/font/window size) from config through
    # the allowlisted env pkexec passes on. Set here so BOTH the GUI (`veracage
    # _up`) and a cold CLI `veracage open` get it, without the broker having to.
    cfg = config.load()
    os.environ["VERACAGE_THEME"] = cfg.theme
    os.environ["VERACAGE_FONT_FILE"] = config.font_file(cfg.ui_font)
    os.environ["VERACAGE_FONT_SIZE"] = str(config.base_font_size(cfg.ui_font_size, cfg.ui_font))
    os.environ["VERACAGE_WINDOW_SIZE"] = cfg.window_size
    if cfg.debug:
        os.environ["VERACAGE_DEBUG"] = "1"
    unit = f"veracage-compositor-{secrets.token_hex(3)}.service"
    cmd = [
        "systemd-run", "--user", "--collect", "--quiet",
        f"--unit={unit}", "--description=Veracage compositor",
        "pkexec", HELPER_PATH, "--spawn-compositor", *_forward_env_args(),
        "--", "_compositor", "--socket", "wl-vc",
    ]
    rc = subprocess.run(cmd).returncode
    if rc != 0:
        print(f"veracage: could not start the compositor (rc={rc}).", file=sys.stderr)
        return rc
    # Wait for the compositor WITHOUT racing the human at the polkit prompt.
    # systemd-run returns once the unit is started, but its `pkexec` then blocks
    # on the password dialog for however long you take. Split the wait by the
    # helper's pidfile, which is written only AFTER auth succeeds:
    #   * pidfile absent  -> still authenticating: wait as long as the unit is
    #     alive (no clock bound, 30s or 30min is fine). polkit bounds the dialog;
    #     a cancel makes the unit go inactive/failed, caught below.
    #   * pidfile present -> auth done, compositor initializing: bound to
    #     COMPOSITOR_INIT_TIMEOUT_S so a wedged/crashing compositor is caught.
    init_deadline: float | None = None
    while True:
        if wayland.compositor_is_up():
            return 0
        if _unit_active_state(unit) in _UNIT_DEAD_STATES:
            print("veracage: compositor did not start "
                  "(authentication cancelled or it exited early).", file=sys.stderr)
            return 1
        if wayland.COMPOSITOR_PIDFILE.exists():
            now = time.monotonic()
            if init_deadline is None:
                init_deadline = now + COMPOSITOR_INIT_TIMEOUT_S
            elif now > init_deadline:
                print("veracage: compositor authenticated but its socket never "
                      "appeared (it may have failed to initialize).", file=sys.stderr)
                return 1
        time.sleep(0.15)


def ensure_empty_session() -> None:
    """Bootstrap an empty session leader (front-door scratchpad) if no session is
    live, so apps are launchable inside Veracage before any volume is mounted.
    Same NS + workspace + idmapped exchange + leader as a volume open, minus
    cryptsetup. Idempotent and best-effort: a failure just leaves the Apps menu
    disabled until a volume is mounted, so it never blocks the front door.
    Volumes mounted afterward join THIS session via the add-volume path."""
    if _any_live_session():
        return
    cfg = config.load()
    sid = str(os.getuid())
    apps_list = [{"name": a.name, "exec": a.exec} for a in cfg.apps.values()]
    env_args = _forward_env_args()
    # Same crash-safe teardown as a volume session: ExecStopPost closes any dm the
    # session picked up (none at first; volumes added later append their own).
    exec_stop_post = _exec_stop_post(sid)
    unit = f"veracage-session-{sid}-{secrets.token_hex(3)}.service"
    helper_flags = ["--empty-session", "--session", sid] + _exchange_flags(cfg)
    leader_args = ["_leader", "--apps", json.dumps(apps_list)]
    if cfg.debug:
        leader_args.append("--debug")
    # Detached transient unit (no --pty/--pipe): systemd-run returns once started;
    # the leader runs in the background, pkexec authenticating via the polkit agent
    # (auth_self_keep coalesces this with the compositor's prompt moments earlier).
    cmd = [
        "systemd-run", "--user", "--quiet", "--collect",
        f"--unit={unit}",
        "--description=Veracage session (scratchpad)",
        "--property", exec_stop_post,
        "pkexec", HELPER_PATH, *helper_flags, *env_args,
        "--", *leader_args,
    ]
    rc = subprocess.run(cmd).returncode
    if rc != 0:
        print(f"veracage: could not start the empty session (rc={rc}). "
              "Apps stay disabled until a volume is mounted.", file=sys.stderr)


def cmd_up(args: argparse.Namespace) -> int:
    """Bring up the persistent compositor with NO volume, the empty front door,
    plus an empty session leader so apps are usable as a sandboxed scratchpad.
    Idempotent. The compositor + empty-session pkexecs coalesce into one prompt."""
    rc = ensure_compositor_up()
    if rc != 0:
        return rc
    # The pub dir exists once the compositor is up: give its Apps menu the
    # configured app names (the broker adds icons on top).
    config.publish_apps(config.load())
    # Front-door empty session (best-effort; never fails the front door).
    ensure_empty_session()
    return 0


# ----------------------------------------------------------- open --------

def _session_sockets() -> list[Path]:
    """The session control sockets currently present (live or stale)."""
    try:
        return sorted((Path(os.environ["XDG_RUNTIME_DIR"]) / "veracage"
                       / "sessions").glob("*.sock"))
    except (KeyError, OSError):
        return []


def _any_live_session() -> bool:
    """True if ANY session leader answers a ping, i.e. a shared workspace is
    already up, so this open will JOIN it (the helper's add-volume path) rather
    than bootstrap. Only used to tell the user what actually happened; the
    bootstrap-vs-add decision itself is the helper's."""
    for sp in _session_sockets():
        try:
            if leader.send_request_to(sp, {"cmd": "ping"}).get("ok"):
                return True
        except (OSError, ValueError):
            continue
    return False


def cmd_sync_apps(args: argparse.Namespace) -> int:
    """Push the configured app list into every running session leader
    (`set-apps`), so a Configure-apps save updates the Apps menu of the LIVE
    session, not only the next one. Best-effort: a session that does not
    answer just keeps its old list."""
    cfg = config.load()
    # Also refresh the published pub/ files (app list, font, mimeapps seed):
    # the broker runs _sync-apps on every config.toml change, so this keeps the
    # published state current for both GUI and CLI edits.
    config.publish_apps(cfg)
    apps_list = [{"name": a.name, "exec": a.exec} for a in cfg.apps.values()]
    for sp in _session_sockets():
        try:
            leader.send_request_to(sp, {"cmd": "set-apps", "apps": apps_list})
        except (OSError, ValueError):
            continue
    return 0


def cmd_open(args: argparse.Namespace) -> int:
    cfg = config.load()
    # Apps and volumes are independent (shared-workspace model): a vault mounts
    # even with no apps enabled. It just appears with nothing launched, and the
    # user enables/launches apps from the compositor. (No is_empty() gate.)

    # Every enabled app becomes a toolbar launcher.
    apps_list = [{"name": a.name, "exec": a.exec}
                 for a in cfg.apps.values()]
    first_app = None
    if args.app is not None:
        if args.app not in cfg.apps:
            print(f"veracage: app '{args.app}' is not enabled.\n"
                  f"Enabled: {', '.join(cfg.apps) or '(none)'}\n"
                  f"Run `veracage configure` to add it.",
                  file=sys.stderr)
            return 2
        a = cfg.apps[args.app]
        first_app = {"name": a.name, "exec": a.exec}
    else:
        # No app named: auto-launch a file manager if one is enabled, so mounting
        # a volume lands you in a browser of its contents (nothing else
        # auto-launches).
        fm = next((a for a in cfg.apps.values()
                   if Path(a.exec).name in FILE_MANAGERS), None)
        if fm is not None:
            first_app = {"name": fm.name, "exec": fm.exec}

    vault = Path(args.volume).resolve()
    if not vault.exists():   # file or block device; the helper validates which
        print(f"veracage: volume not found: {vault}", file=sys.stderr)
        return 2

    if args.app is not None:
        cfg.last_used_app = args.app
        config.save(cfg)

    # Shared-workspace session id: one session per human uid. The helper mounts
    # the volume into that session's tmpfs workspace at /vaults/<label> and injects
    # the (label-derived) mountpoint into the leader argv, so the CLI no longer
    # picks a mountpoint. (docs/shared-workspace.md.)
    sid = str(os.getuid())
    backend = cfg.backend_for(str(vault))

    env_args = _forward_env_args()

    # Bring up the ONE persistent compositor FIRST, as its own systemd --user
    # unit, if it isn't already running (the GUI/broker already does this via
    # `veracage _up`). It must be a separate unit, not forked inside this open's
    # session unit: a forked compositor shares the session unit's cgroup and is
    # SIGKILLed when that unit stops (KillMode=control-group), taking down the
    # shared front door, and any other session still using it. pkexec's
    # auth_self_keep coalesces this prompt with the mount's, so a cold open still
    # authenticates once. The mount helper then only VERIFIES the socket exists.
    rc = ensure_compositor_up()
    if rc != 0:
        return rc
    # The compositor is up, so the pub dir exists: publish the app list for its
    # Apps menu (the broker also does this and adds icons; CLI-only users get
    # the names from here).
    config.publish_apps(cfg)

    # Refuse if THIS vault is already mounted in the running session. The reply's
    # `bootstrap_open` distinguishes "the session is alive" (its socket serves
    # for the whole session) from "this volume is still mounted". After a
    # per-volume close of the bootstrap vault it must be reopenable (the helper
    # then takes the add-volume path). Older leaders without the field: refuse
    # (the conservative pre-Phase-5 behavior). The helper independently enforces
    # the duplicate-open guard from the session lock, so this probe is UX only.
    try:
        existing = leader.send_request(str(vault), {"cmd": "list"})
        if existing.get("ok") and existing.get("bootstrap_open", True):
            print(f"veracage: {vault} is already mounted\n"
                  f"Its window is in the compositor - use the toolbar to add apps.",
                  file=sys.stderr)
            return 2
    except FileNotFoundError:
        pass  # no session socket - none running
    except ConnectionRefusedError:
        # Socket file exists but nothing is listening - genuinely stale (the
        # leader died). Remove it and proceed.
        with contextlib.suppress(FileNotFoundError):
            leader.session_socket_path(str(vault)).unlink()
    except (OSError, ValueError, KeyError) as e:
        # Timeout / torn reply / missing runtime dir: a session MAY be alive but
        # busy - do NOT unlink (that would orphan it) or double-mount. Refuse.
        print(f"veracage: couldn't probe for an existing session ({e}). "
              "Refusing rather than risk a double mount.\n"
              "If you're sure no session is running, remove the stale control "
              "socket and retry.", file=sys.stderr)
        return 2

    # The human side resolves the enabled apps from config and passes them to the
    # leader at open (a trusted argv - whoever opens the vault provided the
    # passphrase). The leader publishes them to the compositor toolbar and only
    # ever launches from THIS list (by index on a toolbar click), never a command
    # a control-socket peer supplies. bwrap confines whatever runs.
    # No --mountpoint: the helper computes /vaults/<label> after reading the
    # volume label (post-cryptsetup) and injects it into this leader argv.
    leader_args = ["_leader", "--apps", json.dumps(apps_list)]
    if cfg.debug:
        leader_args.append("--debug")
    if first_app is not None:
        leader_args += ["--first", json.dumps(first_app)]

    # Wrap the launch in a systemd transient *service* (not a scope: scope units
    # reject Exec* properties, so ExecStopPost never registered) so cleanup is
    # guaranteed even if the wrapper is SIGKILL'd - ExecStopPost fires when the
    # unit stops for any reason (graceful exit, panic, OOM, log-out). --pty keeps
    # an interactive terminal so pkexec can still prompt for the password
    # (org.veracage.helper = auth_self_keep) and the session's stdio stays live.
    vh = cleanup.vault_hash(str(vault))
    # Quote the helper path: systemd re-tokenizes the ExecStopPost value on
    # whitespace, so an install/checkout path containing a space would otherwise
    # split into wrong args and the on-stop dismount would silently never run.
    # Session teardown: closes EVERY volume's dm from session-<sid>.lock (the
    # workspace mounts died with the leader NS). Phase 2 = one volume per session.
    exec_stop_post = _exec_stop_post(sid)
    # Unique per invocation: a fixed `veracage-<hash>.service` collides on a retry
    # if a prior attempt left the unit loaded ("Unit ... was already loaded").
    # The vault-hash prefix keeps it identifiable; the random suffix avoids reuse.
    unit = f"veracage-{vh[:8]}-{secrets.token_hex(3)}.service"

    # Terminal CLI: --pty so cryptsetup can prompt for the passphrase on the tty.
    # GUI launcher (--passphrase-stdin): --pipe instead, so the passphrase piped
    # to our stdin flows through to the helper (which reads it for cryptsetup);
    # a GUI has no tty for an interactive prompt.
    helper_flags = ["--source", str(vault), "--backend", backend,
                    "--session", sid]
    if args.passphrase_stdin:
        helper_flags.append("--passphrase-stdin")
    helper_flags += _exchange_flags(cfg)
    cmd = [
        "systemd-run", "--user",
        "--pipe" if args.passphrase_stdin else "--pty",
        "--quiet",
        "--collect",
        f"--unit={unit}",
        f"--description=Veracage session for {vault.name}",
        "--property", exec_stop_post,
        "pkexec",
        HELPER_PATH,
        *helper_flags,
        *env_args,
        "--",
        *leader_args,
    ]

    # The controls now live in the compositor's toolbar (Phase 3), so we no
    # longer spawn a separate agent window - that would be a redundant second
    # floating window. The scriptable `veracage-agent` CLI still exists for
    # automation; the GUI is the in-compositor toolbar.
    joining = _any_live_session()
    rc = subprocess.run(cmd).returncode
    if rc == 0 and joining:
        # The helper took the add-volume path: the volume is mounted into the
        # RUNNING workspace. The --first app still launches: the helper hands
        # the spec to the live leader (launch.req), since the leader argv we
        # built here was never consumed.
        print("veracage: volume added to the running workspace "
              "(already-running apps see it at their next launch).",
              file=sys.stderr)
    return rc


# ------------------------------------------------------------- _leader ----

def cmd_leader(args: argparse.Namespace) -> int:
    """B2 leader: runs inside the private mount NS AS the vault uid (the helper
    dropped to us and passed the control fd via the environment)."""
    apps_list = json.loads(args.apps) if args.apps else []
    first_app = json.loads(args.first) if args.first else None
    return leader.run_leader(args.mountpoint, apps_list, first_app, args.debug)


# ------------------------------------------------------ veracage list / close

def _drop_stale_socket(vault: str) -> None:
    """A socket file with no listener: the leader died without cleanup. Remove it
    so the next probe/open doesn't trip over it (mirrors cmd_open's handling)."""
    with contextlib.suppress(OSError):
        leader.session_socket_path(vault).unlink()


def cmd_list(args: argparse.Namespace) -> int:
    vault = str(Path(args.volume).resolve())
    try:
        reply = leader.send_request(vault, {"cmd": "list"})
    except FileNotFoundError:
        print(f"veracage: no active session for {vault}", file=sys.stderr)
        return 2
    except ConnectionRefusedError:
        _drop_stale_socket(vault)
        print(f"veracage: no active session for {vault} (removed a stale socket)",
              file=sys.stderr)
        return 2
    for entry in reply.get("apps", []):
        print(f"  {entry['pid']:>6}  {entry['app']}")
    return 0


def cmd_close(args: argparse.Namespace) -> int:
    vault = str(Path(args.volume).resolve())
    try:
        leader.send_request(vault, {"cmd": "close"})
    except FileNotFoundError:
        print(f"veracage: no active session for {vault}", file=sys.stderr)
        return 2
    except ConnectionRefusedError:
        _drop_stale_socket(vault)
        print(f"veracage: no active session for {vault} (removed a stale socket)",
              file=sys.stderr)
        return 2
    return 0


def cmd_close_volume(args: argparse.Namespace) -> int:
    """Close ONE volume of the running session (Phase 5). `label` is the workspace
    directory name (a single component). pkexecs the helper's --close-volume,
    which setns'es into the session and unmounts + deferred-closes just that
    volume, leaving the rest running. Driven by the compositor's per-volume close.
    """
    label = args.label
    if not label or "/" in label or label in (".", ".."):
        print(f"veracage: invalid volume label {label!r}", file=sys.stderr)
        return 2
    cmd = ["pkexec", HELPER_PATH, "--close-volume", label, "--session", str(os.getuid())]
    return subprocess.run(cmd).returncode


# ----------------------------------------------------------- main --------

def main() -> int:
    p = argparse.ArgumentParser(prog="veracage")
    sub = p.add_subparsers(dest="cmd", required=True)

    p_open = sub.add_parser("open", help="mount a volume into the Veracage session")
    p_open.add_argument("volume")
    p_open.add_argument("app", nargs="?", default=None,
                        help="app key (see `veracage configure --list`)")
    # The GUI launcher pipes the volume passphrase to our stdin (no tty).
    p_open.add_argument("--passphrase-stdin", action="store_true",
                        help=argparse.SUPPRESS)
    p_open.set_defaults(func=cmd_open)

    p_leader = sub.add_parser("_leader", help=argparse.SUPPRESS)
    p_leader.add_argument("--mountpoint", required=True)
    p_leader.add_argument("--apps", default=None)   # JSON list, for the toolbar
    p_leader.add_argument("--first", default=None)  # JSON spec to auto-launch
    p_leader.add_argument("--debug", action="store_true")  # verbose timing logs
    p_leader.set_defaults(func=cmd_leader)

    p_comp = sub.add_parser("_compositor", help=argparse.SUPPRESS)
    p_comp.add_argument("--socket", required=True)
    p_comp.set_defaults(func=cmd_compositor)

    p_up = sub.add_parser("_up", help=argparse.SUPPRESS)  # bring up the empty compositor
    p_up.set_defaults(func=cmd_up)

    p_sync = sub.add_parser("_sync-apps", help=argparse.SUPPRESS)
    p_sync.set_defaults(func=cmd_sync_apps)

    p_list = sub.add_parser("list", help="list apps in a running session")
    p_list.add_argument("volume")
    p_list.set_defaults(func=cmd_list)

    p_close = sub.add_parser("close", help="close the running session (unmount every volume)")
    p_close.add_argument("volume")
    p_close.set_defaults(func=cmd_close)

    p_cv = sub.add_parser("close-volume", help="unmount one volume of the session")
    p_cv.add_argument("label", help="the volume's label (workspace directory name)")
    p_cv.set_defaults(func=cmd_close_volume)

    configure.add_subparser(sub)

    args = p.parse_args()
    return args.func(args)
