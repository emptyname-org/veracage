.PHONY: help build build-agent test-agent build-compositor test-compositor check-policy install install-dev uninstall uninstall-dev test test-rs test-rs-root lint clean test-vault smoke veracage-user

PREFIX     ?= /usr/local
BINDIR     ?= $(PREFIX)/bin
LIBDIR     ?= $(PREFIX)/lib/veracage
LIBEXEC    ?= $(PREFIX)/libexec/veracage
POLKIT_DIR ?= /usr/share/polkit-1/actions
APPDIR     ?= $(PREFIX)/share/applications
ICONDIR    ?= $(PREFIX)/share/icons/hicolor/256x256/apps
PIXMAPDIR  ?= $(PREFIX)/share/pixmaps
# The installed hicolor/pixmap icon: a plain 256px scale of the master
# (`convert <master> -resize 256x256 Icons/veracage_256.png`). Regenerate after
# replacing the master art.
ICON_SRC   := Icons/veracage_256.png
UDEVDIR    ?= /usr/lib/udev/rules.d
SLEEPDIR   ?= /usr/lib/systemd/system-sleep

# Real install writes under $(PREFIX) / $(POLKIT_DIR) (root-owned), so the
# file-install steps need root. A staged DESTDIR build (packaging) installs
# into a user-writable tree, so no sudo. The `build` prerequisite always runs
# as the invoking user (cached cargo index) - only these steps are privileged.
SUDO := $(if $(DESTDIR),,sudo)

DEV_ROOT   := $(CURDIR)
VENV       := $(DEV_ROOT)/.venv
PY         ?= python3
CARGO      ?= cargo

# The GUI agent needs a MODERN toolchain (rustup); the helper stays on distro
# rustc 1.63. Prefer rustup's cargo if it's installed, else fall back to $(CARGO).
RUSTUP_CARGO := $(HOME)/.cargo/bin/cargo
AGENT_CARGO  ?= $(if $(wildcard $(RUSTUP_CARGO)),$(RUSTUP_CARGO),$(CARGO))

# Where a generated file is staged for validation before it is installed.
POLICY_TMP := $(DEV_ROOT)/.veracage-install.tmp

# The continuation path baked into the helper. install/install-dev override it.
CONT       ?= $(BINDIR)/veracage

HELPER_BIN     := helper-rs/target/release/veracage-helper
AGENT_BIN      := agent-rs/target/release/veracage-agent
COMPOSITOR_BIN := compositor-rs/target/release/veracage-compositor

help:
	@echo 'Targets:'
	@echo '  build           Build the Rust privilege helper (release)'
	@echo '  build-agent     Build the human-side agent (egui GUI + CLI; needs rustup)'
	@echo '  build-compositor  Build the nested compositor (smithay; needs rustup)'
	@echo '  install       Install to $$(PREFIX) [=$(PREFIX)] and wire polkit (uses sudo)'
	@echo '  install-dev   Build helper + point a polkit policy at this checkout (uses sudo)'
	@echo '  test          Run the Python unit tests'
	@echo '  test-rs       Run the Rust helper unit tests (cargo test)'
	@echo '  lint          ruff + mypy (needs the .venv dev deps)'
	@echo '  audit         cargo-audit the 3 dependency trees (needs network)'
	@echo '  deps          what each binary pulls in (cargo tree)'
	@echo '  uninstall / uninstall-dev / clean'
	@echo '  test-vault    Create a throwaway VeraCrypt volume for manual testing'

# --- build ---------------------------------------------------------------
# CONT = the continuation path baked into the helper (the installed CLI).
# The default is GLOBAL, not `build: CONT ?= ...`: a target-specific `?=` on the
# prerequisite wins over the value install/install-dev set for it, which silently
# baked the INSTALLED path into a dev helper (so a dev session ran the installed
# Python, or died at "continuation not executable" on a box without it).
# `?=` still lets `make build CONT=/some/path` override from the command line.
build:
	VERACAGE_CONTINUATION='$(CONT)' $(CARGO) build --release --manifest-path helper-rs/Cargo.toml

# --- agent (GUI + CLI) ---------------------------------------------------
# The human-side agent: egui control window + scriptable CLI, no vault access.
# Static binary - the end user installs nothing extra to run it (no Qt).
build-agent:
	$(AGENT_CARGO) build --release --manifest-path agent-rs/Cargo.toml

# Agent unit tests (pure logic: config parse/clamp, font metrics). No GUI needed.
test-agent:
	$(AGENT_CARGO) test --release --manifest-path agent-rs/Cargo.toml

# --- nested compositor ---------------------------------------------------
# Our own minimal smithay compositor: renders the sandbox apps and owns the
# private host<->sandbox clipboard channel. Needs a modern toolchain (rustup)
# like the agent. It links libxkbcommon, whose linker symlink `libxkbcommon.so`
# normally comes from libxkbcommon-dev - but every desktop already ships the
# runtime `libxkbcommon.so.0`, so if the -dev symlink is absent we synthesize a
# private one under target/ and point the linker at it. No -dev package needed;
# the runtime binary links the standard soname either way.
XKB_DEV := $(firstword $(wildcard /usr/lib/*/libxkbcommon.so /usr/lib/libxkbcommon.so))
XKB_RT  := $(firstword $(wildcard /usr/lib/*/libxkbcommon.so.0 /usr/lib/libxkbcommon.so.0))

build-compositor:
	@if [ -z "$(XKB_DEV)" ] && [ -n "$(XKB_RT)" ]; then \
	  echo "libxkbcommon-dev absent; linking via a private symlink to $(XKB_RT)"; \
	  mkdir -p compositor-rs/target/xkblink; \
	  ln -sf "$(XKB_RT)" compositor-rs/target/xkblink/libxkbcommon.so; \
	  RUSTFLAGS="-L $(CURDIR)/compositor-rs/target/xkblink $$RUSTFLAGS" \
	    $(AGENT_CARGO) build --release --manifest-path compositor-rs/Cargo.toml; \
	else \
	  $(AGENT_CARGO) build --release --manifest-path compositor-rs/Cargo.toml; \
	fi

# Compositor unit tests (pure logic: shortcuts, mono_icons, hint layout). Uses
# the same libxkbcommon linker fallback as build-compositor.
test-compositor:
	@if [ -z "$(XKB_DEV)" ] && [ -n "$(XKB_RT)" ]; then \
	  mkdir -p compositor-rs/target/xkblink; \
	  ln -sf "$(XKB_RT)" compositor-rs/target/xkblink/libxkbcommon.so; \
	  RUSTFLAGS="-L $(CURDIR)/compositor-rs/target/xkblink $$RUSTFLAGS" \
	    $(AGENT_CARGO) test --release --manifest-path compositor-rs/Cargo.toml; \
	else \
	  $(AGENT_CARGO) test --release --manifest-path compositor-rs/Cargo.toml; \
	fi

# --- vault user ----------------------------------------------------------
# The helper presents the vault as (and runs apps as) this dedicated system
# user; it must exist before `veracage open`. Idempotent.
veracage-user:
	@id veracage >/dev/null 2>&1 || sudo useradd -r -M -s /usr/sbin/nologin veracage
	# GPU: membership in `render` lets the compositor and the sandboxed apps open
	# /dev/dri/renderD* for hardware GL (the helper applies it via initgroups).
	# Without it Mesa silently falls back to llvmpipe software rendering.
	@if getent group render >/dev/null 2>&1 && ! id -nG veracage | grep -qw render; then \
	  sudo usermod -aG render veracage; fi
	@echo "veracage user uid: $$(id -u veracage)"

# --- dependency review ---------------------------------------------------
# Also a gate component. Findings that need an UPSTREAM release are recorded in
# tests/audit-reviewed.txt with the reason, so the gate stays green on what has
# been looked at and turns red on anything NEW. No network means SKIP, not fail.
audit:
	@command -v cargo-audit >/dev/null 2>&1 || { \
	  echo 'cargo-audit not installed: cargo install cargo-audit --locked'; exit 2; }
	@rc=0; ign=$$(awk '/^RUSTSEC-/ {printf " --ignore %s", $$1}' tests/audit-reviewed.txt); \
	  for c in helper-rs agent-rs compositor-rs; do \
	  echo "=== $$c ==="; cargo-audit audit --file $$c/Cargo.lock $$ign || rc=1; done; exit $$rc

# What the three binaries actually pull in. Worth a look before a release: this
# is a tool that runs next to decrypted data, and the helper's tree (11 crates)
# is small on purpose while the GUI crates are not.
deps:
	@for c in helper-rs agent-rs compositor-rs; do \
	  echo "=== $$c: $$($(AGENT_CARGO) tree --manifest-path $$c/Cargo.toml --prefix none --no-dedupe 2>/dev/null | sort -u | wc -l) unique crates ==="; \
	  $(AGENT_CARGO) tree --manifest-path $$c/Cargo.toml --duplicates 2>/dev/null | head -40; done

# --- polkit policy check -------------------------------------------------
# polkitd drops a malformed policy file WHOLE and says nothing, so a broken one
# does not fail loudly: it removes both Veracage actions, pkexec falls back to
# org.freedesktop.policykit.exec, and every privileged step (compositor, session,
# the cleanup that must stay passwordless) starts asking for a password. Parse
# the template before either install writes it.
check-policy:
	@$(PY) -c "import xml.etree.ElementTree as ET; ET.parse('install/org.veracage.policy.in')" \
	  || { echo 'install/org.veracage.policy.in is not well-formed XML'; exit 1; }

# install-policy <helper-path> <cleanup-path> <destination>: substitute, parse the
# RESULT (not just the template: a checkout path containing & < > " or | corrupts
# the substitution, and polkitd drops a malformed file whole and silently), then
# install it root-owned 0644. `tee` used to hide sed's exit status behind the
# pipe, and nothing set the mode.
define install-policy
	sed -e 's|@HELPER@|$(1)|g' -e 's|@CLEANUP@|$(2)|g' \
	    install/org.veracage.policy.in > "$(POLICY_TMP)"
	$(PY) -c "import xml.etree.ElementTree as ET; ET.parse('$(POLICY_TMP)')" \
	  || { echo 'generated polkit policy is not well-formed XML (check the paths for & < > " |)'; rm -f "$(POLICY_TMP)"; exit 1; }
	$(SUDO) install -m 0644 "$(POLICY_TMP)" "$(3)"
	@rm -f "$(POLICY_TMP)"
endef

# --- real install --------------------------------------------------------
install: CONT := $(BINDIR)/veracage
install: check-policy build build-agent build-compositor
	# Dedicated vault system user (skipped for staged/packaged DESTDIR builds,
	# where a package postinst should create it instead).
	@if [ -z "$(DESTDIR)" ]; then id veracage >/dev/null 2>&1 || sudo useradd -r -M -s /usr/sbin/nologin veracage; fi
	# render-group membership -> hardware GL for the compositor + sandboxed apps.
	@if [ -z "$(DESTDIR)" ] && getent group render >/dev/null 2>&1 && ! id -nG veracage | grep -qw render; then \
	  sudo usermod -aG render veracage; fi
	# Python package
	$(SUDO) install -d "$(DESTDIR)$(LIBDIR)"
	# cp -r into an existing dir nests (…/veracage/veracage) and leaves the old
	# copy in place; wipe first so reinstall actually replaces the package.
	$(SUDO) rm -rf "$(DESTDIR)$(LIBDIR)/veracage"
	$(SUDO) cp -r src/veracage "$(DESTDIR)$(LIBDIR)/veracage"
	$(SUDO) find "$(DESTDIR)$(LIBDIR)" -name __pycache__ -type d -prune -exec rm -rf {} +
	# Privileged helpers
	$(SUDO) install -d "$(DESTDIR)$(LIBEXEC)"
	$(SUDO) install -m 0755 "$(HELPER_BIN)" "$(DESTDIR)$(LIBEXEC)/veracage-helper"
	sed 's|@LIBDIR@|$(LIBDIR)|g' install/veracage-cleanup.in | $(SUDO) tee "$(DESTDIR)$(LIBEXEC)/veracage-cleanup" >/dev/null
	$(SUDO) chmod 0755 "$(DESTDIR)$(LIBEXEC)/veracage-cleanup"
	# Human-side agent (GUI window + scriptable CLI `veracage-agent`)
	$(SUDO) install -d "$(DESTDIR)$(BINDIR)"
	$(SUDO) install -m 0755 "$(AGENT_BIN)" "$(DESTDIR)$(BINDIR)/veracage-agent"
	# Nested compositor (renders the sandbox + owns the private clipboard channel)
	$(SUDO) install -m 0755 "$(COMPOSITOR_BIN)" "$(DESTDIR)$(BINDIR)/veracage-compositor"
	# Launcher
	sed -e 's|@LIBDIR@|$(LIBDIR)|g' -e 's|@LIBEXEC@|$(LIBEXEC)|g' \
	    -e 's|@AGENT@|$(BINDIR)/veracage-agent|g' \
	    -e 's|@COMPOSITOR@|$(BINDIR)/veracage-compositor|g' \
	    install/veracage.in | $(SUDO) tee "$(DESTDIR)$(BINDIR)/veracage" >/dev/null
	$(SUDO) chmod 0755 "$(DESTDIR)$(BINDIR)/veracage"
	# polkit policy (paths must match the launcher's VERACAGE_* env)
	$(SUDO) install -d "$(DESTDIR)$(POLKIT_DIR)"
	$(call install-policy,$(LIBEXEC)/veracage-helper,$(LIBEXEC)/veracage-cleanup,$(DESTDIR)$(POLKIT_DIR)/org.veracage.policy)
	# Launcher .desktop app + icon (Name=Veracage, the bird-in-a-cage icon)
	$(SUDO) install -d "$(DESTDIR)$(APPDIR)" "$(DESTDIR)$(ICONDIR)" "$(DESTDIR)$(PIXMAPDIR)"
	sed 's|@AGENT@|$(BINDIR)/veracage-agent|g' install/veracage.desktop.in \
	    | $(SUDO) tee "$(DESTDIR)$(APPDIR)/veracage.desktop" >/dev/null
	$(SUDO) chmod 0644 "$(DESTDIR)$(APPDIR)/veracage.desktop"
	$(SUDO) install -m 0644 "$(ICON_SRC)" "$(DESTDIR)$(ICONDIR)/veracage.png"
	$(SUDO) install -m 0644 "$(ICON_SRC)" "$(DESTDIR)$(PIXMAPDIR)/veracage.png"
	# udev rule: keep the decrypted vault dm devices out of the drive menu and
	# out of /dev/disk/by-label,by-uuid. Numbered 57 (was 99, which ran too late
	# to stop the symlinks), so drop the old file if this box still has it.
	$(SUDO) install -d "$(DESTDIR)$(UDEVDIR)"
	$(SUDO) rm -f "$(DESTDIR)$(UDEVDIR)/99-veracage.rules"
	$(SUDO) install -m 0644 install/57-veracage.rules "$(DESTDIR)$(UDEVDIR)/57-veracage.rules"
	@[ -n "$(DESTDIR)" ] || sudo udevadm control --reload 2>/dev/null || true
	# system-sleep hook: dismount every session before the machine sleeps so the
	# dm-crypt key never sits in RAM across suspend/hibernate.
	$(SUDO) install -d "$(DESTDIR)$(SLEEPDIR)"
	sed 's|@LIBDIR@|$(LIBDIR)|g' install/veracage-sleep.in | $(SUDO) tee "$(DESTDIR)$(SLEEPDIR)/veracage" >/dev/null
	$(SUDO) chmod 0755 "$(DESTDIR)$(SLEEPDIR)/veracage"
	@[ -n "$(DESTDIR)" ] || { command -v update-desktop-database >/dev/null 2>&1 && sudo update-desktop-database "$(APPDIR)" 2>/dev/null; } || true
	@[ -n "$(DESTDIR)" ] || { command -v gtk-update-icon-cache >/dev/null 2>&1 && sudo gtk-update-icon-cache -f -t "$(PREFIX)/share/icons/hicolor" 2>/dev/null; } || true
	@echo 'Installed to $(PREFIX). Launch "Veracage" from your app menu, or run: veracage configure'

# --- dev install: polkit points at this checkout -------------------------
install-dev: CONT := $(DEV_ROOT)/src/bin/veracage
install-dev: check-policy veracage-user build
	chmod +x "$(DEV_ROOT)/src/bin/veracage" "$(DEV_ROOT)/helpers/veracage-cleanup"
	$(call install-policy,$(DEV_ROOT)/$(HELPER_BIN),$(DEV_ROOT)/helpers/veracage-cleanup,$(POLKIT_DIR)/org.veracage.policy)
	# The udev rule and the system-sleep hook are host-global, not $(PREFIX)-scoped,
	# and both FAIL OPEN when absent: without the rule the decrypted volume's label
	# and UUID are published under /dev/disk to every local user and UDisks offers
	# it in the drive menu; without the hook there is no suspend teardown at all.
	# A dev box needs them exactly as much as a real install does.
	sudo install -d "$(UDEVDIR)"
	sudo rm -f "$(UDEVDIR)/99-veracage.rules"
	sudo install -m 0644 install/57-veracage.rules "$(UDEVDIR)/57-veracage.rules"
	sudo udevadm control --reload 2>/dev/null || true
	sudo install -d "$(SLEEPDIR)"
	sed 's|@LIBDIR@|$(DEV_ROOT)/src|g' install/veracage-sleep.in > "$(POLICY_TMP)"
	sudo install -m 0755 "$(POLICY_TMP)" "$(SLEEPDIR)/veracage"
	@rm -f "$(POLICY_TMP)"
	@echo 'Dev install done. The CLI auto-detects the built Rust helper.'
	@echo 'Run: $(DEV_ROOT)/src/bin/veracage configure'
	@echo ''
	@echo '*** SECURITY: this points polkit at TWO programs in this USER-WRITABLE'
	@echo '*** checkout and runs them as ROOT. The cleanup one (helpers/veracage-cleanup,'
	@echo '*** and every file it imports from src/veracage/) is authorised PASSWORDLESS,'
	@echo '*** so anything that can write this checkout has root for the asking - no'
	@echo '*** prompt, no `veracage open` needed. Use ONLY on a single-user or'
	@echo '*** disposable box - NEVER on a shared/multi-user machine. Use `make'
	@echo '*** install` (root-owned /usr/local) for anything real.'

uninstall:
	sudo rm -f "$(POLKIT_DIR)/org.veracage.policy"
	sudo rm -f "$(APPDIR)/veracage.desktop" "$(ICONDIR)/veracage.png" "$(PIXMAPDIR)/veracage.png"
	sudo rm -f "$(UDEVDIR)/57-veracage.rules" "$(UDEVDIR)/99-veracage.rules"; sudo udevadm control --reload 2>/dev/null || true
	sudo rm -f "$(SLEEPDIR)/veracage"
	sudo rm -rf "$(LIBDIR)" "$(LIBEXEC)" "$(BINDIR)/veracage" \
	    "$(BINDIR)/veracage-agent" "$(BINDIR)/veracage-compositor"

# Removes what install-dev put on the HOST. The udev rule and the sleep hook are
# global paths a real `make install` writes too, so this only touches them when
# there is no real install left behind them (a stale rule would keep the
# decrypted volume out of /dev/disk, which is harmless, but a sleep hook whose
# $(LIBDIR) is gone would run and fail on every suspend).
uninstall-dev:
	sudo rm -f "$(POLKIT_DIR)/org.veracage.policy"
	@if [ -d "$(LIBDIR)/veracage" ]; then \
	  echo "keeping the udev rule + sleep hook: $(LIBDIR)/veracage is still installed"; \
	else \
	  sudo rm -f "$(UDEVDIR)/57-veracage.rules"; \
	  sudo udevadm control --reload 2>/dev/null || true; \
	  sudo rm -f "$(SLEEPDIR)/veracage"; \
	  echo "removed the udev rule and the system-sleep hook"; \
	fi

# --- dev workflow --------------------------------------------------------
test:
	$(PY) -m pytest

test-rs:
	$(CARGO) test --manifest-path helper-rs/Cargo.toml

# Root-only Rust tests (idmap mount). Build AS THE USER (cached crate index),
# then run the test binary under sudo directly - never `sudo cargo`, which uses
# root's empty CARGO_HOME and re-fetches the ~900 MB index.
test-rs-root:
	$(CARGO) test --manifest-path helper-rs/Cargo.toml --no-run
	sudo "$$(ls -t helper-rs/target/debug/deps/veracage_helper-* | grep -vE '\.(d|so)$$' | head -1)" --ignored --nocapture

lint:
	$(VENV)/bin/ruff check src/ tests/ && $(VENV)/bin/mypy

clean:
	$(CARGO) clean --manifest-path helper-rs/Cargo.toml 2>/dev/null || true
	$(AGENT_CARGO) clean --manifest-path agent-rs/Cargo.toml 2>/dev/null || true
	$(AGENT_CARGO) clean --manifest-path compositor-rs/Cargo.toml 2>/dev/null || true

# Create a 50 MB VeraCrypt vault for manual/integration testing.
# Password 'veracage-test'.
test-vault:
	@if [ -e /tmp/veracage-test.vc ]; then \
		echo '/tmp/veracage-test.vc already exists; remove it to recreate.'; exit 1; \
	fi
	veracrypt --text --create /tmp/veracage-test.vc \
	          --volume-type=normal --size=50M \
	          --encryption=AES --hash=SHA-512 \
	          --filesystem=ext4 --pim=0 --keyfiles="" \
	          --password=vvv --random-source=/dev/urandom
	@echo 'Created /tmp/veracage-test.vc (password: vvv)'

smoke:
	$(DEV_ROOT)/src/bin/veracage open /tmp/veracage-test.vc
