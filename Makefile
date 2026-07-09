.PHONY: help build build-agent build-compositor install install-dev uninstall uninstall-dev test test-rs test-rs-root lint clean test-vault smoke veracage-user

PREFIX     ?= /usr/local
BINDIR     ?= $(PREFIX)/bin
LIBDIR     ?= $(PREFIX)/lib/veracage
LIBEXEC    ?= $(PREFIX)/libexec/veracage
POLKIT_DIR ?= /usr/share/polkit-1/actions
APPDIR     ?= $(PREFIX)/share/applications
ICONDIR    ?= $(PREFIX)/share/icons/hicolor/256x256/apps
PIXMAPDIR  ?= $(PREFIX)/share/pixmaps
ICON_SRC   := Icons/veracage_icon_turquoise_transparent_corners.png
UDEVDIR    ?= /usr/lib/udev/rules.d
SLEEPDIR   ?= /usr/lib/systemd/system-sleep

# Real install writes under $(PREFIX) / $(POLKIT_DIR) (root-owned), so the
# file-install steps need root. A staged DESTDIR build (packaging) installs
# into a user-writable tree, so no sudo. The `build` prerequisite always runs
# as the invoking user (cached cargo index) — only these steps are privileged.
SUDO := $(if $(DESTDIR),,sudo)

DEV_ROOT   := $(CURDIR)
VENV       := $(DEV_ROOT)/.venv
PY         ?= python3
CARGO      ?= cargo

# The GUI agent needs a MODERN toolchain (rustup); the helper stays on distro
# rustc 1.63. Prefer rustup's cargo if it's installed, else fall back to $(CARGO).
RUSTUP_CARGO := $(HOME)/.cargo/bin/cargo
AGENT_CARGO  ?= $(if $(wildcard $(RUSTUP_CARGO)),$(RUSTUP_CARGO),$(CARGO))

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
	@echo '  uninstall / uninstall-dev / clean'
	@echo '  test-vault    Create a throwaway VeraCrypt vault for manual testing'

# --- build ---------------------------------------------------------------
# CONT = the continuation path baked into the helper (the installed CLI).
# Target-specific CONT (below) propagates to this prerequisite.
build: CONT ?= $(BINDIR)/veracage
build:
	VERACAGE_CONTINUATION='$(CONT)' $(CARGO) build --release --manifest-path helper-rs/Cargo.toml

# --- agent (GUI + CLI) ---------------------------------------------------
# The human-side agent: egui control window + scriptable CLI, no vault access.
# Static binary — the end user installs nothing extra to run it (no Qt).
build-agent:
	$(AGENT_CARGO) build --release --manifest-path agent-rs/Cargo.toml

# --- nested compositor ---------------------------------------------------
# Our own minimal smithay compositor: renders the sandbox apps and owns the
# private host<->sandbox clipboard channel. Needs a modern toolchain (rustup)
# like the agent. It links libxkbcommon, whose linker symlink `libxkbcommon.so`
# normally comes from libxkbcommon-dev — but every desktop already ships the
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

# --- vault user ----------------------------------------------------------
# The helper presents the vault as (and runs apps as) this dedicated system
# user; it must exist before `veracage open`. Idempotent.
veracage-user:
	@id veracage >/dev/null 2>&1 || sudo useradd -r -M -s /usr/sbin/nologin veracage
	@echo "veracage user uid: $$(id -u veracage)"

# --- real install --------------------------------------------------------
install: CONT := $(BINDIR)/veracage
install: build build-agent build-compositor
	# Dedicated vault system user (skipped for staged/packaged DESTDIR builds,
	# where a package postinst should create it instead).
	@if [ -z "$(DESTDIR)" ]; then id veracage >/dev/null 2>&1 || sudo useradd -r -M -s /usr/sbin/nologin veracage; fi
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
	sed -e 's|@HELPER@|$(LIBEXEC)/veracage-helper|g' \
	    -e 's|@CLEANUP@|$(LIBEXEC)/veracage-cleanup|g' \
	    install/org.veracage.policy.in | $(SUDO) tee "$(DESTDIR)$(POLKIT_DIR)/org.veracage.policy" >/dev/null
	# Launcher .desktop app + icon (Name=Veracage, the bird-in-a-cage icon)
	$(SUDO) install -d "$(DESTDIR)$(APPDIR)" "$(DESTDIR)$(ICONDIR)" "$(DESTDIR)$(PIXMAPDIR)"
	sed 's|@AGENT@|$(BINDIR)/veracage-agent|g' install/veracage.desktop.in \
	    | $(SUDO) tee "$(DESTDIR)$(APPDIR)/veracage.desktop" >/dev/null
	$(SUDO) chmod 0644 "$(DESTDIR)$(APPDIR)/veracage.desktop"
	$(SUDO) install -m 0644 "$(ICON_SRC)" "$(DESTDIR)$(ICONDIR)/veracage.png"
	$(SUDO) install -m 0644 "$(ICON_SRC)" "$(DESTDIR)$(PIXMAPDIR)/veracage.png"
	# udev rule: hide the decrypted vault dm devices from UDisks/the drive menu.
	$(SUDO) install -d "$(DESTDIR)$(UDEVDIR)"
	$(SUDO) install -m 0644 install/99-veracage.rules "$(DESTDIR)$(UDEVDIR)/99-veracage.rules"
	@[ -n "$(DESTDIR)" ] || sudo udevadm control --reload 2>/dev/null || true
	# system-sleep hook: dismount every session before the machine sleeps so the
	# dm-crypt key never sits in RAM across suspend/hibernate.
	$(SUDO) install -d "$(DESTDIR)$(SLEEPDIR)"
	sed 's|@LIBDIR@|$(LIBDIR)|g' install/veracage-sleep.in | $(SUDO) tee "$(DESTDIR)$(SLEEPDIR)/veracage" >/dev/null
	$(SUDO) chmod 0755 "$(DESTDIR)$(SLEEPDIR)/veracage"
	@[ -n "$(DESTDIR)" ] || command -v update-desktop-database >/dev/null 2>&1 && sudo update-desktop-database "$(APPDIR)" 2>/dev/null || true
	@[ -n "$(DESTDIR)" ] || command -v gtk-update-icon-cache >/dev/null 2>&1 && sudo gtk-update-icon-cache -f -t "$(PREFIX)/share/icons/hicolor" 2>/dev/null || true
	@echo 'Installed to $(PREFIX). Launch "Veracage" from your app menu, or run: veracage configure'

# --- dev install: polkit points at this checkout -------------------------
install-dev: CONT := $(DEV_ROOT)/src/bin/veracage
install-dev: veracage-user build
	chmod +x "$(DEV_ROOT)/src/bin/veracage" "$(DEV_ROOT)/helpers/veracage-cleanup"
	sed -e 's|@HELPER@|$(DEV_ROOT)/$(HELPER_BIN)|g' \
	    -e 's|@CLEANUP@|$(DEV_ROOT)/helpers/veracage-cleanup|g' \
	    install/org.veracage.policy.in | sudo tee "$(POLKIT_DIR)/org.veracage.policy" >/dev/null
	sudo mkdir -p /run/veracage
	@echo 'Dev install done. The CLI auto-detects the built Rust helper.'
	@echo 'Run: $(DEV_ROOT)/src/bin/veracage configure'
	@echo ''
	@echo '*** SECURITY: dev install points polkit at a helper in this USER-WRITABLE'
	@echo '*** checkout and runs it as ROOT. Any process running as you can overwrite'
	@echo '*** it and gain root on the next `veracage open`. Use ONLY on a single-user'
	@echo '*** or disposable box — NEVER on a shared/multi-user machine. Use `make'
	@echo '*** install` (root-owned /usr/local) for anything real.'

uninstall:
	sudo rm -f "$(POLKIT_DIR)/org.veracage.policy"
	sudo rm -f "$(APPDIR)/veracage.desktop" "$(ICONDIR)/veracage.png" "$(PIXMAPDIR)/veracage.png"
	sudo rm -f "$(UDEVDIR)/99-veracage.rules"; sudo udevadm control --reload 2>/dev/null || true
	sudo rm -f "$(SLEEPDIR)/veracage"
	sudo rm -rf "$(LIBDIR)" "$(LIBEXEC)" "$(BINDIR)/veracage" \
	    "$(BINDIR)/veracage-agent" "$(BINDIR)/veracage-compositor"

uninstall-dev:
	sudo rm -f "$(POLKIT_DIR)/org.veracage.policy"

# --- dev workflow --------------------------------------------------------
test:
	$(PY) -m pytest

test-rs:
	$(CARGO) test --manifest-path helper-rs/Cargo.toml

# Root-only Rust tests (idmap mount). Build AS THE USER (cached crate index),
# then run the test binary under sudo directly — never `sudo cargo`, which uses
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
