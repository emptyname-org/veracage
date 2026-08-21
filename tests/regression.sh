#!/usr/bin/env bash
# Veracage regression suite - run after every change.
#
#   bash tests/regression.sh            # full suite
#   bash tests/regression.sh --no-smoke # skip the headless compositor smoke
#
# Each component reports PASS / FAIL / SKIP. SKIP = tooling absent on this host
# (not a failure). The script exits non-zero iff any component FAILs, so it is
# safe to gate commits/CI on it. Designed to run both locally and on a headless
# box - it auto-detects what it can run.
set -u
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"
export PATH="$HOME/.cargo/bin:$PATH"

WANT_SMOKE=1
[ "${1:-}" = "--no-smoke" ] && WANT_SMOKE=0

declare -A RESULT
ORDER=()
bold(){ printf '\033[1m%s\033[0m\n' "$*"; }
# component <name> <fn>: fn returns 0=PASS, 2=SKIP, other=FAIL
component(){
  local name="$1" fn="$2"; ORDER+=("$name")
  printf '\n'; bold "=== $name ==="
  local rc; "$fn"; rc=$?
  case $rc in 0) RESULT[$name]=PASS;; 2) RESULT[$name]=SKIP;; *) RESULT[$name]=FAIL;; esac
  echo "--> ${RESULT[$name]}"
}
have(){ command -v "$1" >/dev/null 2>&1; }

# ---------------------------------------------------------------- rust ------
c_helper() { # privilege helper: pinned to DISTRO rustc (1.63 on Debian 12).
  # Pin BOTH the distro cargo and the distro rustc. cargo resolves `rustc` from
  # PATH, which this script prepends rustup to, so pinning cargo alone quietly
  # tested the rustup toolchain and masked exactly the MSRV breakage this check
  # exists to catch (it did: four `let...else` and a char-array `trim_matches`
  # reached the tree unnoticed).
  local cargo=/usr/bin/cargo rustc=/usr/bin/rustc
  [ -x "$cargo" ] || cargo="$(command -v cargo)"
  [ -x "$rustc" ] || rustc="$(command -v rustc)"
  [ -n "$cargo" ] || { echo "no cargo"; return 2; }
  echo "helper toolchain: $("$rustc" --version 2>/dev/null || echo unknown)"
  RUSTC="$rustc" "$cargo" test --manifest-path helper-rs/Cargo.toml
}
c_compositor_build() {
  have make || { echo "no make"; return 2; }
  # `|| return 1`: GNU make exits 2 on a build/test FAILURE, and 2 is this
  # script's code for "tooling absent", so a real failure used to be reported as
  # a SKIP.
  make build-compositor || return 1
  make test-compositor || return 1
}
c_agent_build() {
  have make || { echo "no make"; return 2; }
  make build-agent || return 1
  make test-agent || return 1
}

# -------------------------------------------------------------- python ------
PY="${PY:-python3}"
c_py_tests() {
  # Prefer the project venv (has pytest); fall back to system python.
  local py="$PY"
  [ -x "$ROOT/.venv/bin/python" ] && py="$ROOT/.venv/bin/python"
  have "$py" || { echo "no python"; return 2; }
  "$py" -c 'import pytest' 2>/dev/null || { echo "pytest not importable"; return 2; }
  # unit tests don't need root/GUI; run them. Integration tests that need a real
  # vault/Wayland are marked in tests/integration/MANUAL.md and skipped here.
  # PYTHONPATH=src so `from veracage import ...` resolves without an editable install.
  PYTHONPATH="$ROOT/src" "$py" -m pytest -q tests/unit
}

c_helper_contract() { # the privilege-boundary tests, against the BUILT helper
  local py="$PY"
  [ -x "$ROOT/.venv/bin/python" ] && py="$ROOT/.venv/bin/python"
  have "$py" || { echo "no python"; return 2; }
  "$py" -c 'import pytest' 2>/dev/null || { echo "pytest not importable"; return 2; }
  [ -f "$ROOT/helper-rs/target/release/veracage-helper" ] || {
    echo "helper not built (release)"; return 2; }
  # SECURITY.md names these as the guard on the helper's argument contract, and
  # nothing ran them: pytest.ini's testpaths stops at tests/unit. They need no
  # root, no vault and no Wayland (every case fails before the helper forks).
  PYTHONPATH="$ROOT/src" "$py" -m pytest -q tests/integration/test_helper_security.py
}
c_py_lint() {
  # Prefer the project venv; else system ruff/mypy on PATH.
  local ruff mypy
  if [ -x "$ROOT/.venv/bin/ruff" ]; then ruff="$ROOT/.venv/bin/ruff"; mypy="$ROOT/.venv/bin/mypy"
  elif have ruff && have mypy; then ruff=ruff; mypy=mypy
  else echo "no ruff/mypy"; return 2; fi
  "$ruff" check src/ tests/ && "$mypy"
}

# ------------------------------------------------- headless compositor ------
# Nest our compositor in a headless weston and exercise the runtime paths that
# don't need human interaction: startup (no panic from any global/handler), every
# advertised global, a real client mapping a toplevel, clean SIGTERM. (The
# clipboard is in-process + focus-gated now, so its round-trip needs a real
# session - not headless-testable.)
c_headless_smoke() {
  [ "$WANT_SMOKE" = 1 ] || { echo "disabled (--no-smoke)"; return 2; }
  have weston || { echo "no weston"; return 2; }
  local COMP="$ROOT/compositor-rs/target/release/veracage-compositor"
  [ -x "$COMP" ] || { echo "compositor not built (run c_compositor_build first)"; return 2; }
  have weston-simple-shm || { echo "no weston demo clients"; return 2; }
  have weston-terminal || { echo "no weston-terminal (the map check needs it)"; return 2; }

  # Run the whole nested exercise in a SUBSHELL so its teardown trap and temp
  # state are fully contained and can never leak to the outer runner.
  (
    XR="$(mktemp -d /tmp/vc-smoke.XXXXXX)"; export XDG_RUNTIME_DIR="$XR"; chmod 700 "$XR"
    WPID=""; VPID=""
    trap '
      for p in "$VPID" "$WPID"; do [ -n "$p" ] && [ "$p" != 0 ] && kill -9 "$p" 2>/dev/null; done
      rm -rf "$XR"
    ' EXIT

    # weston 10 (Debian 12) wants the module file name; 11+ also accepts the
    # short one. Naming it "headless" alone made every local run fail to start a
    # host and report SKIP, which is how the client-flush regression got past
    # this gate. Try the module name first, then the short form.
    weston --backend=headless-backend.so --socket=wl-host --width=1600 --height=1000 >"$XR/weston.out" 2>&1 &
    WPID=$!
    for i in $(seq 1 20); do [ -S "$XR/wl-host" ] && break; sleep 0.25; done
    if [ ! -S "$XR/wl-host" ]; then
      kill "$WPID" 2>/dev/null
      weston --backend=headless --socket=wl-host --width=1600 --height=1000 >"$XR/weston.out" 2>&1 &
      WPID=$!
      for i in $(seq 1 40); do [ -S "$XR/wl-host" ] && break; sleep 0.25; done
    fi
    [ -S "$XR/wl-host" ] || { echo "FAIL: weston host socket never appeared"; tail -8 "$XR/weston.out"; exit 1; }
    echo "  weston host up"

    WAYLAND_DISPLAY=wl-host LIBGL_ALWAYS_SOFTWARE=1 GALLIUM_DRIVER=llvmpipe \
      "$COMP" --socket wl-vc >"$XR/comp.out" 2>&1 &
    VPID=$!
    for i in $(seq 1 80); do [ -S "$XR/wl-vc" ] && break; kill -0 "$VPID" 2>/dev/null || break; sleep 0.25; done
    kill -0 "$VPID" 2>/dev/null || { echo "FAIL: compositor exited at startup"; tail -20 "$XR/comp.out"; exit 1; }
    [ -S "$XR/wl-vc" ] || { echo "FAIL: wl-vc socket absent"; tail -20 "$XR/comp.out"; exit 1; }
    echo "  compositor up; wl-vc present"

    # Every global we expect must be advertised to a connecting client.
    WAYLAND_DISPLAY=wl-vc WAYLAND_DEBUG=1 timeout 3 weston-simple-shm >"$XR/client.out" 2>&1
    want="xdg_wm_base wl_seat wl_shm wl_data_device_manager zxdg_decoration_manager_v1 org_kde_kwin_server_decoration_manager wp_viewporter wp_fractional_scale_manager_v1 zwp_primary_selection_device_manager_v1 wp_cursor_shape_manager_v1"
    missing=""
    for g in $want; do grep -q "$g" "$XR/client.out" || missing="$missing $g"; done
    [ -z "$missing" ] || { echo "FAIL: globals not advertised:$missing"; exit 1; }
    echo "  all $(echo $want | wc -w) expected globals advertised"

    # A real toolkit-ish client maps a toplevel: exercises new_toplevel (placement,
    # bounds, focus-on-map -> data-device + primary focus), decoration negotiation,
    # and the commit path. Compositor must survive it.
    WAYLAND_DISPLAY=wl-vc timeout 3 weston-terminal >"$XR/term.out" 2>&1 &
    sleep 2
    kill -0 "$VPID" 2>/dev/null || { echo "FAIL: compositor died when a client mapped a toplevel"; tail -20 "$XR/comp.out"; exit 1; }
    echo "  client mapped a toplevel; compositor alive"

    # Clean SIGTERM shutdown must not hang or panic.
    kill -TERM "$VPID" 2>/dev/null
    for i in $(seq 1 20); do kill -0 "$VPID" 2>/dev/null || break; sleep 0.1; done
    if kill -0 "$VPID" 2>/dev/null; then echo "FAIL: compositor did not exit on SIGTERM"; exit 1; fi
    VPID=""
    echo "  compositor exited cleanly on SIGTERM"

    grep -qiE 'panic|panicked' "$XR/comp.out" && { echo "FAIL: panic in compositor stderr"; grep -iE 'panic|panicked' "$XR/comp.out" | head; exit 1; }
    echo "  no panics"
    exit 0
  )
  return $?
}

# ------------------------------------------------------------- run ----------
bold "Veracage regression suite  (root: $ROOT)"
echo "toolchain: $(cargo --version 2>/dev/null || echo 'no cargo') / $($PY --version 2>&1)"

component "rust: helper build+test"  c_helper
component "rust: compositor build"   c_compositor_build
component "rust: agent build"        c_agent_build
component "python: unit tests"       c_py_tests
component "helper argument contract" c_helper_contract
component "python: lint (ruff+mypy)" c_py_lint
component "headless compositor smoke" c_headless_smoke

# ------------------------------------------------------------- summary ------
# A SKIP of a CORE component (a build or the unit tests) means the environment
# can't actually run the gate - treat it as a failure, not a silent pass.
CORE_RE='helper build|compositor build|agent build|unit tests|lint|argument contract'
printf '\n'; bold "================ SUMMARY ================"
fails=0
for name in "${ORDER[@]}"; do
  r="${RESULT[$name]}"
  case "$r" in
    PASS) c='\033[32m';;
    SKIP) if printf '%s' "$name" | grep -qE "$CORE_RE"; then c='\033[31m'; r='SKIP!'; fails=$((fails+1)); else c='\033[33m'; fi;;
    *) c='\033[31m'; fails=$((fails+1));;
  esac
  printf "  ${c}%-6s\033[0m %s\n" "$r" "$name"
done
printf '\n(SKIP! = a core component was skipped → counts as failure)\n\n'
if [ "$fails" -eq 0 ]; then bold "ALL GREEN ($fails failures)"; exit 0; else bold "$fails FAILURE(S)"; exit 1; fi
