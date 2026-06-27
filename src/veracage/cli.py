"""CLI entry: argparse + subcommand dispatch."""
from __future__ import annotations

import argparse
import os
import secrets
import subprocess
import sys
from pathlib import Path

from . import cleanup, config, configure, session
from .sandbox import bwrap_command  # noqa: F401  (kept for downstream tests)
from .wayland import nested_weston, WestonStartFailed  # noqa: F401


HELPER_PATH = os.environ.get(
    "VERACAGE_HELPER",
    "/home/pq/Claude/veracage/helpers/veracage-helper",
)
CLEANUP_HELPER_PATH = os.environ.get(
    "VERACAGE_CLEANUP_HELPER",
    "/home/pq/Claude/veracage/helpers/veracage-cleanup",
)

# Env vars to forward through pkexec (which strips the environment).
# WAYLAND_DISPLAY + XDG_RUNTIME_DIR are required for the nested weston to
# pick the wayland-backend and place its socket; the rest are useful for
# theming / locale.
_FORWARD_ENV = (
    "WAYLAND_DISPLAY",
    "XDG_RUNTIME_DIR",
    "XDG_SESSION_TYPE",
    "DISPLAY",
    "LANG",
)


def _entry_script() -> str:
    """Path to src/bin/veracage — passed to the helper as --continuation."""
    # __file__ = src/veracage/cli.py  →  src/bin/veracage
    return str(Path(__file__).resolve().parent.parent / "bin" / "veracage")


# ----------------------------------------------------------- open --------

def cmd_open(args: argparse.Namespace) -> int:
    cfg = config.load()
    if cfg.is_empty():
        print("veracage: no apps configured — running auto-detection.\n"
              "(run `veracage configure` later to choose explicitly.)",
              file=sys.stderr)
        rc = configure._auto(args)
        if rc != 0:
            return rc
        cfg = config.load()
        if cfg.is_empty():
            print("veracage: no catalog apps installed on this system.\n"
                  "Install at least one (e.g. `apt install kate`) and re-run.",
                  file=sys.stderr)
            return 2

    app_key = args.app or cfg.last_used_app or next(iter(cfg.apps))
    if app_key not in cfg.apps:
        print(f"veracage: app '{app_key}' is not enabled.\n"
              f"Enabled: {', '.join(cfg.apps) or '(none)'}\n"
              f"Run `veracage configure` to add it.",
              file=sys.stderr)
        return 2

    vault = Path(args.vault).resolve()
    if not vault.is_file():
        print(f"veracage: vault not found: {vault}", file=sys.stderr)
        return 2

    # Remember last choice
    cfg.last_used_app = app_key
    config.save(cfg)

    mountpoint = Path(f"/run/veracage/{secrets.token_hex(8)}")

    env_args: list[str] = []
    for k in _FORWARD_ENV:
        v = os.environ.get(k)
        if v is not None:
            env_args += ["--setenv", f"{k}={v}"]

    # Refuse if a session is already running for this vault.
    try:
        existing = session.send_request(str(vault), {"cmd": "list"})
        if existing.get("ok"):
            print(f"veracage: a session is already open for {vault}\n"
                  f"Use `veracage exec {vault} {app_key}` to add an app to it.",
                  file=sys.stderr)
            return 2
    except FileNotFoundError:
        pass  # no session running — good
    except OSError:
        # Stale socket file. Remove and proceed.
        from .session import session_socket_path
        with __import__("contextlib").suppress(FileNotFoundError):
            session_socket_path(str(vault)).unlink()

    # Wrap the launch in a systemd transient scope so cleanup is guaranteed
    # even if the wrapper is SIGKILL'd. ExecStopPost fires whenever the
    # scope is destroyed (graceful exit, panic, OOM, log-out).
    vh = cleanup.vault_hash(str(vault))
    exec_stop_post = (
        f"ExecStopPost=pkexec {CLEANUP_HELPER_PATH} --vault-hash {vh}"
    )
    scope_unit = f"veracage-{vh[:8]}.scope"

    cmd = [
        "systemd-run", "--user", "--scope", "--quiet",
        "--collect",
        f"--unit={scope_unit}",
        f"--description=Veracage session for {Path(vault).name}",
        "--property", exec_stop_post,
        "pkexec",
        HELPER_PATH,
        "--vault", str(vault),
        "--mountpoint", str(mountpoint),
        "--user", str(os.getuid()),
        "--group", str(os.getgid()),
        *env_args,
        "--continuation", _entry_script(),
        "--",
        "_continue", "--mountpoint", str(mountpoint),
                     "--vault", str(vault),
                     "--app", app_key,
    ]
    return subprocess.run(cmd).returncode


# ---------------------------------------------------------- _continue ----

def cmd_continue(args: argparse.Namespace) -> int:
    """Run inside the private mount NS as the user. Becomes the session leader."""
    return session.run_session(args.mountpoint, args.vault, args.app)


# ----------------------------------------------- veracage exec / list / close

def cmd_exec(args: argparse.Namespace) -> int:
    vault = str(Path(args.vault).resolve())
    cfg = config.load()
    if args.app not in cfg.apps:
        print(f"veracage: app '{args.app}' not enabled\n"
              f"Enabled: {', '.join(cfg.apps) or '(none)'}", file=sys.stderr)
        return 2
    try:
        reply = session.send_request(vault, {"cmd": "exec", "app": args.app})
    except FileNotFoundError:
        print(f"veracage: no active session for {vault}\n"
              f"Run `veracage open {vault}` first.", file=sys.stderr)
        return 2
    if not reply.get("ok"):
        print(f"veracage: exec failed: {reply.get('error')}", file=sys.stderr)
        return 1
    return 0


def cmd_list(args: argparse.Namespace) -> int:
    vault = str(Path(args.vault).resolve())
    try:
        reply = session.send_request(vault, {"cmd": "list"})
    except FileNotFoundError:
        print(f"veracage: no active session for {vault}", file=sys.stderr)
        return 2
    for entry in reply.get("apps", []):
        print(f"  {entry['pid']:>6}  {entry['app']}")
    return 0


def cmd_close(args: argparse.Namespace) -> int:
    vault = str(Path(args.vault).resolve())
    try:
        session.send_request(vault, {"cmd": "close"})
    except FileNotFoundError:
        print(f"veracage: no active session for {vault}", file=sys.stderr)
        return 2
    return 0


def cmd_agent(args: argparse.Namespace) -> int:
    from . import agent
    return agent.run(args.vault, args.mountpoint, args.weston_socket)


# ----------------------------------------------------------- main --------

def main() -> int:
    p = argparse.ArgumentParser(prog="veracage")
    sub = p.add_subparsers(dest="cmd", required=True)

    p_open = sub.add_parser("open", help="open a vault and run an app")
    p_open.add_argument("vault")
    p_open.add_argument("app", nargs="?", default=None,
                        help="app key (see `veracage configure --list`)")
    p_open.set_defaults(func=cmd_open)

    p_cont = sub.add_parser("_continue", help=argparse.SUPPRESS)
    p_cont.add_argument("--mountpoint", required=True)
    p_cont.add_argument("--vault", required=True)
    p_cont.add_argument("--app", required=True)
    p_cont.set_defaults(func=cmd_continue)

    p_exec = sub.add_parser("exec", help="add an app to a running session")
    p_exec.add_argument("vault")
    p_exec.add_argument("app")
    p_exec.set_defaults(func=cmd_exec)

    p_list = sub.add_parser("list", help="list apps in a running session")
    p_list.add_argument("vault")
    p_list.set_defaults(func=cmd_list)

    p_close = sub.add_parser("close", help="close a running session")
    p_close.add_argument("vault")
    p_close.set_defaults(func=cmd_close)

    p_agent = sub.add_parser("_agent", help=argparse.SUPPRESS)
    p_agent.add_argument("--vault", required=True)
    p_agent.add_argument("--mountpoint", required=True)
    p_agent.add_argument("--weston-socket", required=True)
    p_agent.set_defaults(func=cmd_agent)

    configure.add_subparser(sub)

    args = p.parse_args()
    return args.func(args)
