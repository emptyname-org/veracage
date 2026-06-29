.PHONY: help build install install-dev uninstall uninstall-dev test test-rs test-rs-root lint clean test-vault smoke veracage-user

PREFIX     ?= /usr/local
BINDIR     ?= $(PREFIX)/bin
LIBDIR     ?= $(PREFIX)/lib/veracage
LIBEXEC    ?= $(PREFIX)/libexec/veracage
POLKIT_DIR ?= /usr/share/polkit-1/actions

DEV_ROOT   := $(CURDIR)
VENV       := $(DEV_ROOT)/.venv
PY         ?= python3
CARGO      ?= cargo

HELPER_BIN := helper-rs/target/release/veracage-helper

help:
	@echo 'Targets:'
	@echo '  build         Build the Rust privilege helper (release)'
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

# --- vault user ----------------------------------------------------------
# The helper presents the vault as (and runs apps as) this dedicated system
# user; it must exist before `veracage open`. Idempotent.
veracage-user:
	@id veracage >/dev/null 2>&1 || sudo useradd -r -M -s /usr/sbin/nologin veracage
	@echo "veracage user uid: $$(id -u veracage)"

# --- real install --------------------------------------------------------
install: CONT := $(BINDIR)/veracage
install: build
	# Dedicated vault system user (skipped for staged/packaged DESTDIR builds,
	# where a package postinst should create it instead).
	@if [ -z "$(DESTDIR)" ]; then id veracage >/dev/null 2>&1 || sudo useradd -r -M -s /usr/sbin/nologin veracage; fi
	# Python package
	install -d "$(DESTDIR)$(LIBDIR)"
	cp -r src/veracage "$(DESTDIR)$(LIBDIR)/veracage"
	find "$(DESTDIR)$(LIBDIR)" -name __pycache__ -type d -prune -exec rm -rf {} +
	# Privileged helpers
	install -d "$(DESTDIR)$(LIBEXEC)"
	install -m 0755 "$(HELPER_BIN)" "$(DESTDIR)$(LIBEXEC)/veracage-helper"
	sed 's|@LIBDIR@|$(LIBDIR)|g' install/veracage-cleanup.in > "$(DESTDIR)$(LIBEXEC)/veracage-cleanup"
	chmod 0755 "$(DESTDIR)$(LIBEXEC)/veracage-cleanup"
	# Launcher
	install -d "$(DESTDIR)$(BINDIR)"
	sed -e 's|@LIBDIR@|$(LIBDIR)|g' -e 's|@LIBEXEC@|$(LIBEXEC)|g' \
	    install/veracage.in > "$(DESTDIR)$(BINDIR)/veracage"
	chmod 0755 "$(DESTDIR)$(BINDIR)/veracage"
	# polkit policy (paths must match the launcher's VERACAGE_* env)
	install -d "$(DESTDIR)$(POLKIT_DIR)"
	sed -e 's|@HELPER@|$(LIBEXEC)/veracage-helper|g' \
	    -e 's|@CLEANUP@|$(LIBEXEC)/veracage-cleanup|g' \
	    install/org.veracage.policy.in > "$(DESTDIR)$(POLKIT_DIR)/org.veracage.policy"
	@echo 'Installed to $(PREFIX). Run: veracage configure'

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

uninstall:
	sudo rm -f "$(POLKIT_DIR)/org.veracage.policy"
	sudo rm -rf "$(LIBDIR)" "$(LIBEXEC)" "$(BINDIR)/veracage"

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
	          --password=veracage-test --random-source=/dev/urandom
	@echo 'Created /tmp/veracage-test.vc (password: veracage-test)'

smoke:
	$(DEV_ROOT)/src/bin/veracage open /tmp/veracage-test.vc
