.PHONY: help install-dev uninstall-dev test-vault smoke

PREFIX     ?= /usr/local
LIBEXEC    ?= $(PREFIX)/libexec/veracage
BIN        ?= $(PREFIX)/bin
POLKIT_DIR ?= /usr/share/polkit-1/actions

# Dev paths (used by the polkit policy in install/)
DEV_ROOT   := /home/pq/Claude/veracage

help:
	@echo 'Targets:'
	@echo '  install-dev    Install polkit policy pointing at $(DEV_ROOT)'
	@echo '  uninstall-dev  Remove the polkit policy'
	@echo '  test           Run unit tests via pytest'
	@echo '  test-vault     Create a 50 MB test VeraCrypt vault at /tmp/veracage-test.vc'
	@echo '  smoke          Open the test vault with kate'

test:
	python3 -m pytest

# Make scripts executable, install polkit policy that points at DEV_ROOT.
install-dev:
	chmod +x $(DEV_ROOT)/src/bin/veracage \
	         $(DEV_ROOT)/helpers/veracage-helper \
	         $(DEV_ROOT)/helpers/veracage-cleanup
	sudo install -m 0644 $(DEV_ROOT)/install/org.veracage.policy $(POLKIT_DIR)/org.veracage.policy
	sudo mkdir -p /run/veracage
	@echo 'Installed.'
	@echo 'Run: $(DEV_ROOT)/src/bin/veracage configure   (pick which apps to enable)'
	@echo '     $(DEV_ROOT)/src/bin/veracage open <vault.vc> [app]'

uninstall-dev:
	sudo rm -f $(POLKIT_DIR)/org.veracage.policy

# Create a 50 MB plain-format VeraCrypt vault (for testing only).
# Password 'veracage-test'.
test-vault:
	@if [ -e /tmp/veracage-test.vc ]; then \
		echo '/tmp/veracage-test.vc already exists; remove it to recreate.'; exit 1; \
	fi
	dd if=/dev/zero of=/tmp/veracage-test.vc bs=1M count=50 status=progress
	@echo
	@echo 'Now format it interactively with veracrypt --text --create:'
	@echo '  veracrypt --text --create /tmp/veracage-test.vc \'
	@echo '            --volume-type=normal --size=50M \'
	@echo '            --encryption=AES --hash=SHA-512 \'
	@echo '            --filesystem=ext4 --pim=0 --keyfiles=" " \'
	@echo '            --random-source=/dev/urandom'
	@echo
	@echo 'Or use cryptsetup tcrypt for a Veracage-style volume.'

smoke:
	$(DEV_ROOT)/src/bin/veracage open /tmp/veracage-test.vc
