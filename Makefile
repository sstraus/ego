# The development Mac resolves Cargo through the shared mbx shim.
export PATH := $(HOME)/Library/Application Support/mbx/bin:$(PATH)

.PHONY: bump

bump:
	python3 scripts/bump-version.py
	cargo check --workspace

# The local release script is available only in the private checkout.
ifneq ($(wildcard scripts/local-release.sh),)
.PHONY: release-local
release-local:
	just changelog-check
	bash scripts/local-release.sh
endif
