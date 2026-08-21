#!/usr/bin/env python3
"""Break a guard on purpose and check the unit suite notices.

    python3 tests/mutation_check.py

Each entry below disables one security- or teardown-critical behaviour in a COPY
of the tree (never the working tree) and runs `pytest tests/unit` against it. A
mutation that SURVIVES means the suite claims to protect that guard and does not:
the code was broken and the tests stayed green.

This exists because an audit found a dozen such guards: `_sanitize_label` could
`return raw`, `_terminate_children` could return immediately, the rmtree
symlink-attack guard could be `if True`, `--clearenv` could be dropped from the
bwrap line, and the suite passed every time. Not part of `tests/regression.sh`
(it re-runs the suite once per mutation, so it takes minutes); run it after
touching anything on this list.
"""
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
# Prefer the project venv (it has pytest), like tests/regression.sh does.
_VENV = REPO / ".venv/bin/python"
PY = str(_VENV) if _VENV.is_file() else sys.executable

# (name, file, old, new)
MUTATIONS = [
    ("sandbox: /etc bound wholesale", "src/veracage/sandbox.py",
     '        "--ro-bind-try", "/etc/fonts", "/etc/fonts",',
     '        "--ro-bind-try", "/etc", "/etc",\n        "--ro-bind-try", "/etc/fonts", "/etc/fonts",'),
    ("sandbox: --clearenv removed", "src/veracage/sandbox.py",
     '        "--clearenv",\n', '\n'),
    ("sandbox: /usr writable", "src/veracage/sandbox.py",
     '        "--ro-bind", "/usr", "/usr",', '        "--bind", "/usr", "/usr",'),
    ("sandbox: host home bound in", "src/veracage/sandbox.py",
     '        "--bind", workspace, "/vaults",',
     '        "--bind", str(Path.home()), "/host-home",\n        "--bind", workspace, "/vaults",'),
    ("sandbox: env inherited wholesale", "src/veracage/sandbox.py",
     '    for var in ("LANG", "LANGUAGE", "LC_ALL", "LC_CTYPE", "LC_TIME", "LC_NUMERIC",\n                "XCURSOR_THEME", "XCURSOR_SIZE"):',
     '    for var in list(os.environ):'),
    # ---- leader ----
    ("leader: label sanitizer neutered", "src/veracage/leader.py",
     '    cleaned = "".join(c if c.isprintable() else " " for c in raw).strip()\n    return cleaned[:64] or "Volume"',
     '    return raw'),
    ("leader: app socket world-connectable", "src/veracage/leader.py",
     '        os.chmod(sock_path, 0o700)', '        os.chmod(sock_path, 0o777)'),
    ("leader: terminate_children does nothing", "src/veracage/leader.py",
     '    for pid in list(state.children):\n        with contextlib.suppress(ProcessLookupError):\n            os.kill(pid, signal.SIGTERM)',
     '    return\n    for pid in list(state.children):\n        with contextlib.suppress(ProcessLookupError):\n            os.kill(pid, signal.SIGTERM)'),
    ("leader: no SIGKILL escalation", "src/veracage/leader.py",
     '            with contextlib.suppress(ProcessLookupError):\n                os.kill(pid, signal.SIGKILL)',
     '            pass'),
    # ---- cleanup ----
    ("cleanup: rmtree guard bypassed", "src/veracage/cleanup.py",
     '        if not run_dir.is_symlink() and shutil.rmtree.avoids_symlink_attacks:',
     '        if True:'),
    ("cleanup: flock never taken", "src/veracage/cleanup.py",
     '    _lockf = _take_flock(p.with_suffix(".flock"))', '    _lockf = None'),
    ("cleanup: --lock re-added", "src/veracage/cleanup.py",
     '    p.add_argument("--session", required=True,',
     '    p.add_argument("--lock")\n    p.add_argument("--session", required=True,'),
    ("cleanup: zombie reads as alive", "src/veracage/cleanup.py",
     '    if len(rest) <= 19 or rest[0] == "Z":', '    if len(rest) <= 19:'),
    ("cleanup: close_dm raises on OSError", "src/veracage/cleanup.py",
     '        except OSError as e:\n', '        except ValueError as e:\n'),
    # ---- config ----
    ("config: shortcut validation removed", "src/veracage/config.py",
     '            if isinstance(v, str) and _valid_keybind(v):',
     '            if isinstance(v, str):'),
    ("config: theme validation removed", "src/veracage/config.py",
     '    if theme not in ("light", "dark", "system"):', '    if False:'),
    ("config: a string counts as a bool", "src/veracage/config.py",
     '    if isinstance(val, bool):\n        return val',
     '    if True:\n        return bool(val)'),
    ("config: save is not atomic", "src/veracage/config.py",
     '    tmp = p.with_name(f"{p.name}.{os.getpid()}.tmp")\n    tmp.write_text("\\n".join(lines))\n    tmp.replace(p)',
     '    p.write_text("\\n".join(lines))'),
    ("config: publish_apps key filter removed", "src/veracage/config.py",
     '        if a.key and len(a.key) <= 64 and "/" not in a.key and a.key != ".."',
     '        if a.key'),
    ("config: app key written unquoted", "src/veracage/config.py",
     "        lines += [f'[apps.\"{_esc(key)}\"]',", '        lines += [f"[apps.{key}]",'),
    # ---- cli ----
    ("cli: ExecStopPost path unquoted", "src/veracage/cli.py",
     "    return f'ExecStopPost=pkexec \"{CLEANUP_HELPER_PATH}\" --session {sid}'",
     "    return f'ExecStopPost=pkexec {CLEANUP_HELPER_PATH} --session {sid}'"),
    ("cli: volume path not canonicalized", "src/veracage/cli.py",
     '    vault = Path(args.volume).resolve()', '    vault = Path(args.volume)'),
    ("cli: double-mount refusal removed", "src/veracage/cli.py",
     '        print(f"veracage: couldn\'t probe for an existing session ({e}). "',
     '        pass\n    if False:\n        print(f"veracage: couldn\'t probe for an existing session ({e}). "'),
    # ---- sleep hook ----
    ("sleep: dm_present always false", "src/veracage/sleep_hook.py",
     '    return bool(dm_name) and Path(f"/dev/mapper/{dm_name}").exists()',
     '    return False'),
    ("sleep: wait_dms_gone always true", "src/veracage/sleep_hook.py",
     '    deadline = time.monotonic() + timeout\n    while time.monotonic() < deadline:',
     '    return True\n    deadline = time.monotonic() + timeout\n    while time.monotonic() < deadline:'),
    ("sleep: force path closes before the kill", "src/veracage/sleep_hook.py",
     '    if leader is not None and pid_alive(leader):\n        with contextlib.suppress(ProcessLookupError):\n            os.kill(leader, signal.SIGKILL)\n    _wait_dms_gone(dm_names, FORCE_SECONDS)\n    cleanup.cleanup_session(lock_path)',
     '    cleanup.cleanup_session(lock_path)\n    if leader is not None and pid_alive(leader):\n        with contextlib.suppress(ProcessLookupError):\n            os.kill(leader, signal.SIGKILL)\n    _wait_dms_gone(dm_names, FORCE_SECONDS)'),
    ("sleep: residual-dm warning removed", "src/veracage/sleep_hook.py",
     '    still = [dm for dm in dm_names if dm_present(dm)]', '    still = []'),
    ("sleep: phase gate removed", "src/veracage/sleep_hook.py",
     '    if phase != "pre" or state not in SLEEP_STATES:', '    if False:'),
    ("sleep: root gate removed", "src/veracage/sleep_hook.py",
     '    if os.geteuid() != 0:', '    if False:'),
    ("sleep: grace shorter than the leader's", "src/veracage/sleep_hook.py",
     'GRACE_SECONDS = leader.TERMINATE_GRACE + 1.0', 'GRACE_SECONDS = 6.0'),
]


def run(tree: Path) -> bool:
    """True if the suite passes in `tree`."""
    r = subprocess.run([PY, "-m", "pytest", "-q", "tests/unit"],
                       cwd=tree, capture_output=True, text=True)
    return r.returncode == 0


def main() -> int:
    survived = []
    with tempfile.TemporaryDirectory(prefix="vc-mutate-") as tmp:
        base = Path(tmp) / "tree"
        shutil.copytree(REPO, base, symlinks=True,
                        ignore=shutil.ignore_patterns(".git", "target", ".venv",
                                                      "__pycache__", "*.pyc",
                                                      "VPS-ACCESS", "prototype",
                                                      "Icons", ".mypy_cache",
                                                      ".ruff_cache", ".pytest_cache"))
        if not run(base):
            print("BASELINE FAILS - fix that first")
            return 2
        print(f"baseline green ({len(MUTATIONS)} mutations to try)\n")
        for name, rel, old, new in MUTATIONS:
            f = base / rel
            src = f.read_text()
            if src.count(old) != 1:
                print(f"  SKIP    {name}  ({src.count(old)} matches)")
                continue
            f.write_text(src.replace(old, new))
            passed = run(base)
            f.write_text(src)
            if passed:
                print(f"  SURVIVED  {name}   <-- the suite does not protect this")
                survived.append(name)
            else:
                print(f"  caught    {name}")
    print()
    print(f"{len(survived)} survived" if survived else "every mutation was caught")
    return 1 if survived else 0


if __name__ == "__main__":
    sys.exit(main())
