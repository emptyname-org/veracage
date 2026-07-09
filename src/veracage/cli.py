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
from pathlib import Path

from . import cleanup, config, configure, leader
from .sandbox import bwrap_command  # noqa: F401  (kept for downstream tests)

# Privileged helpers. An install (`make install`) sets VERACAGE_HELPER /
# VERACAGE_CLEANUP_HELPER in the generated launcher to point at $LIBEXEC.
# For a source checkout we derive paths from the repo root: prefer the built
# Rust mount helper if present, else the Python reference helper. Whatever
# this resolves to MUST match the polkit policy's exec.path.
_REPO_ROOT = Path(__file__).resolve().parents[2]


def _default_mount_helper() -> str:
    rust = _REPO_ROOT / "helper-rs" / "target" / "release" / "veracage-helper"
    if rust.is_file():
        return str(rust)
    return str(_REPO_ROOT / "helpers" / "veracage-helper")


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


# ----------------------------------------------------------- open --------

def cmd_open(args: argparse.Namespace) -> int:
    cfg = config.load()
    if cfg.is_empty():
        print("veracage: no apps enabled yet.\n"
              "Add one with `veracage configure --add <binary>` (e.g. kate, dolphin),\n"
              "or open the launcher's Configure window, then re-run.",
              file=sys.stderr)
        return 2

    # Every enabled app becomes a toolbar launcher; nothing is auto-launched
    # unless one is named explicitly (`veracage open <vault> <app>`).
    apps_list = [{"name": a.name, "exec": a.exec, "args": a.args}
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
        first_app = {"name": a.name, "exec": a.exec, "args": a.args}

    vault = Path(args.vault).resolve()
    if not vault.exists():   # file or block device; the helper validates which
        print(f"veracage: vault not found: {vault}", file=sys.stderr)
        return 2

    if args.app is not None:
        cfg.last_used_app = args.app
        config.save(cfg)

    mountpoint = Path(f"/run/veracage/{secrets.token_hex(8)}")
    gpu = cfg.gpu_for(str(vault))
    backend = cfg.backend_for(str(vault))

    env_args = _forward_env_args()

    # The mount helper (one pkexec, below) also brings up the ONE persistent
    # compositor if it isn't already running — so opening a vault is a single
    # authorization, and every vault's apps render into that one compositor.

    # Refuse if a session is already running for this vault.
    try:
        existing = leader.send_request(str(vault), {"cmd": "list"})
        if existing.get("ok"):
            print(f"veracage: a session is already open for {vault}\n"
                  f"Its window is in the compositor — use the toolbar to add apps.",
                  file=sys.stderr)
            return 2
    except FileNotFoundError:
        pass  # no session socket — none running
    except ConnectionRefusedError:
        # Socket file exists but nothing is listening — genuinely stale (the
        # leader died). Remove it and proceed.
        with contextlib.suppress(FileNotFoundError):
            leader.session_socket_path(str(vault)).unlink()
    except (OSError, ValueError, KeyError) as e:
        # Timeout / torn reply / missing runtime dir: a session MAY be alive but
        # busy — do NOT unlink (that would orphan it) or double-mount. Refuse.
        print(f"veracage: couldn't probe for an existing session ({e}); "
              "refusing rather than risk a double mount.\n"
              "If you're sure none is open, remove the stale control socket and retry.",
              file=sys.stderr)
        return 2

    # The human side resolves the enabled apps from config and passes them to the
    # leader at open (a trusted argv — whoever opens the vault provided the
    # passphrase). The leader publishes them to the compositor toolbar and only
    # ever launches from THIS list (by index on a toolbar click), never a command
    # a control-socket peer supplies. bwrap confines whatever runs.
    leader_args = ["_leader", "--mountpoint", str(mountpoint),
                   "--apps", json.dumps(apps_list)]
    if first_app is not None:
        leader_args += ["--first", json.dumps(first_app)]
    if gpu:
        leader_args.append("--gpu")

    # Wrap the launch in a systemd transient *service* (not a scope: scope units
    # reject Exec* properties, so ExecStopPost never registered) so cleanup is
    # guaranteed even if the wrapper is SIGKILL'd — ExecStopPost fires when the
    # unit stops for any reason (graceful exit, panic, OOM, log-out). --pty keeps
    # an interactive terminal so pkexec can still prompt for the password
    # (org.veracage.helper = auth_self_keep) and the session's stdio stays live.
    vh = cleanup.vault_hash(str(vault))
    # Quote the helper path: systemd re-tokenizes the ExecStopPost value on
    # whitespace, so an install/checkout path containing a space would otherwise
    # split into wrong args and the on-stop dismount would silently never run.
    exec_stop_post = f'ExecStopPost=pkexec "{CLEANUP_HELPER_PATH}" --vault-hash {vh}'
    # Unique per invocation: a fixed `veracage-<hash>.service` collides on a retry
    # if a prior attempt left the unit loaded ("Unit ... was already loaded").
    # The vault-hash prefix keeps it identifiable; the random suffix avoids reuse.
    unit = f"veracage-{vh[:8]}-{secrets.token_hex(3)}.service"

    # Terminal CLI: --pty so cryptsetup can prompt for the passphrase on the tty.
    # GUI launcher (--passphrase-stdin): --pipe instead, so the passphrase piped
    # to our stdin flows through to the helper (which reads it for cryptsetup);
    # a GUI has no tty for an interactive prompt.
    helper_flags = ["--source", str(vault), "--backend", backend,
                    "--mountpoint", str(mountpoint)]
    if args.passphrase_stdin:
        helper_flags.append("--passphrase-stdin")
    # Shared exchange folder: ensure ~/Veracage/Exchange exists (we own it) and
    # pass it to the helper, which idmap-mounts it into the sandbox at /exchange.
    # The helper re-validates ownership; disabled per-config via `exchange = false`.
    if cfg.exchange:
        xdir = Path.home() / "Veracage" / "Exchange"
        try:
            xdir.mkdir(parents=True, exist_ok=True)
            xdir.chmod(0o700)
            helper_flags += ["--exchange", str(xdir)]
        except OSError as e:
            print(f"veracage: could not prepare the exchange folder ({e}); "
                  "continuing without it.", file=sys.stderr)
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
    # longer spawn a separate agent window — that would be a redundant second
    # floating window. The scriptable `veracage-agent` CLI still exists for
    # automation; the GUI is the in-compositor toolbar.
    return subprocess.run(cmd).returncode


# ------------------------------------------------------------- _leader ----

def cmd_leader(args: argparse.Namespace) -> int:
    """B2 leader: runs inside the private mount NS AS the vault uid (the helper
    dropped to us and passed the control fd via the environment)."""
    apps_list = json.loads(args.apps) if args.apps else []
    first_app = json.loads(args.first) if args.first else None
    return leader.run_leader(args.mountpoint, args.gpu, apps_list, first_app)


# ------------------------------------------------------ veracage list / close

def cmd_list(args: argparse.Namespace) -> int:
    vault = str(Path(args.vault).resolve())
    try:
        reply = leader.send_request(vault, {"cmd": "list"})
    except FileNotFoundError:
        print(f"veracage: no active session for {vault}", file=sys.stderr)
        return 2
    for entry in reply.get("apps", []):
        print(f"  {entry['pid']:>6}  {entry['app']}")
    return 0


def cmd_close(args: argparse.Namespace) -> int:
    vault = str(Path(args.vault).resolve())
    try:
        leader.send_request(vault, {"cmd": "close"})
    except FileNotFoundError:
        print(f"veracage: no active session for {vault}", file=sys.stderr)
        return 2
    return 0


# ----------------------------------------------------------- main --------

def main() -> int:
    p = argparse.ArgumentParser(prog="veracage")
    sub = p.add_subparsers(dest="cmd", required=True)

    p_open = sub.add_parser("open", help="open a vault and run an app")
    p_open.add_argument("vault")
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
    p_leader.add_argument("--gpu", action="store_true")
    p_leader.set_defaults(func=cmd_leader)

    p_comp = sub.add_parser("_compositor", help=argparse.SUPPRESS)
    p_comp.add_argument("--socket", required=True)
    p_comp.set_defaults(func=cmd_compositor)

    p_list = sub.add_parser("list", help="list apps in a running session")
    p_list.add_argument("vault")
    p_list.set_defaults(func=cmd_list)

    p_close = sub.add_parser("close", help="close a running session")
    p_close.add_argument("vault")
    p_close.set_defaults(func=cmd_close)

    configure.add_subparser(sub)

    args = p.parse_args()
    return args.func(args)
