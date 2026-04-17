# Makefile for fix-engine cross-compilation
#
# Usage:
#   make build                  # Build for current host platform (release)
#   make build-all              # Build for all supported targets
#   make build-<target>         # Build for a specific target triple
#   make release                # Build + package all targets into dist/
#   make release-<target>       # Build + package a specific target
#   make check-deps             # Verify cross-compilation prerequisites
#   make clean                  # Remove build artifacts
#   make clean-dist             # Remove dist/ packages only
#   make list-targets           # Show all supported targets
#   make help                   # Show this help
#
# Prerequisites:
#   - Rust toolchain (rustup + cargo)
#   - cargo-zigbuild (cargo install cargo-zigbuild): for cross-compilation
#   - zig: required by cargo-zigbuild as the cross-linker
#   - rustup targets: rustup target add <target> for each desired target
#   - tar, zip: for release packaging
#
# Notes:
#   - macOS targets can only be built on macOS (Apple SDK not redistributable)
#   - x86_64-pc-windows-msvc can only be built on Windows
#   - All other targets use cargo-zigbuild (no containers needed)
#   - Native host target uses plain cargo (faster, no zig overhead)
#   - reqwest TLS deps (ring, OpenSSL) are handled by zig's cross-linker

# ──────────────────────────────────────────────
# Project settings
# ──────────────────────────────────────────────
BINARY_NAME   := fix-engine-cli
PACKAGE_NAME  := fix-engine
VERSION       := $(shell grep '^version' Cargo.toml | head -1 | sed 's/.*"\(.*\)"/\1/')

# ──────────────────────────────────────────────
# Host detection
# ──────────────────────────────────────────────
UNAME_S := $(shell uname -s)
UNAME_M := $(shell uname -m)

ifeq ($(UNAME_S),Darwin)
  ifeq ($(UNAME_M),arm64)
    HOST_TARGET := aarch64-apple-darwin
  else
    HOST_TARGET := x86_64-apple-darwin
  endif
else ifeq ($(UNAME_S),Linux)
  ifeq ($(UNAME_M),aarch64)
    HOST_TARGET := aarch64-unknown-linux-gnu
  else
    HOST_TARGET := x86_64-unknown-linux-gnu
  endif
else ifneq (,$(findstring MINGW,$(UNAME_S))$(findstring MSYS,$(UNAME_S))$(findstring CYGWIN,$(UNAME_S)))
  HOST_TARGET := x86_64-pc-windows-msvc
else
  HOST_TARGET := unknown
endif

# ──────────────────────────────────────────────
# Supported targets
# ──────────────────────────────────────────────
LINUX_TARGETS := \
  x86_64-unknown-linux-gnu \
  x86_64-unknown-linux-musl \
  aarch64-unknown-linux-gnu \
  aarch64-unknown-linux-musl

MACOS_TARGETS := \
  x86_64-apple-darwin \
  aarch64-apple-darwin

WINDOWS_TARGETS := \
  x86_64-pc-windows-gnu \
  x86_64-pc-windows-msvc

ALL_TARGETS := $(LINUX_TARGETS) $(MACOS_TARGETS) $(WINDOWS_TARGETS)

# ──────────────────────────────────────────────
# Buildable targets for the current host
# - macOS targets require macOS host (Apple SDK not redistributable)
# - MSVC targets require Windows host
# ──────────────────────────────────────────────
ifeq ($(UNAME_S),Darwin)
  BUILDABLE_TARGETS := $(LINUX_TARGETS) $(MACOS_TARGETS) x86_64-pc-windows-gnu
else ifeq ($(UNAME_S),Linux)
  BUILDABLE_TARGETS := $(LINUX_TARGETS) x86_64-pc-windows-gnu
else ifneq (,$(findstring MINGW,$(UNAME_S))$(findstring MSYS,$(UNAME_S))$(findstring CYGWIN,$(UNAME_S)))
  BUILDABLE_TARGETS := $(LINUX_TARGETS) $(WINDOWS_TARGETS)
else
  BUILDABLE_TARGETS := $(ALL_TARGETS)
endif

# ──────────────────────────────────────────────
# Build tool selection
# - Native host target: plain cargo (no zig overhead)
# - Cross targets: cargo zigbuild (zig handles C cross-linking)
# - macOS from non-macOS: error (needs Apple SDK)
# - MSVC from non-Windows: error (needs MSVC toolchain)
# ──────────────────────────────────────────────
define select_build_cmd
$(if $(filter $(1),$(HOST_TARGET)),cargo build,\
$(if $(and $(filter Darwin,$(UNAME_S)),$(filter $(1),$(MACOS_TARGETS))),cargo build,\
$(if $(filter $(1),x86_64-pc-windows-msvc),$(error x86_64-pc-windows-msvc can only be built on Windows. Use x86_64-pc-windows-gnu for cross-compilation from $(UNAME_S)),\
$(if $(filter $(1),$(MACOS_TARGETS)),$(error macOS targets can only be built on macOS. Current host: $(UNAME_S)),\
cargo zigbuild))))
endef

# Binary extension for target
define binary_ext
$(if $(findstring windows,$(1)),.exe,)
endef

# Archive extension for target
define archive_ext
$(if $(findstring windows,$(1)),zip,tar.gz)
endef

# ──────────────────────────────────────────────
# Directories
# ──────────────────────────────────────────────
DIST_DIR := dist

# ──────────────────────────────────────────────
# Cargo flags
# ──────────────────────────────────────────────
CARGO_FLAGS := --release

# ──────────────────────────────────────────────
# Phony targets
# ──────────────────────────────────────────────
.PHONY: help build build-all release check-deps clean clean-dist list-targets \
        $(addprefix build-,$(ALL_TARGETS)) \
        $(addprefix release-,$(ALL_TARGETS))

# ──────────────────────────────────────────────
# Default target
# ──────────────────────────────────────────────
.DEFAULT_GOAL := help

# ──────────────────────────────────────────────
# Help
# ──────────────────────────────────────────────
help:
	@echo "$(PACKAGE_NAME) v$(VERSION) — Cross-compilation Makefile"
	@echo ""
	@echo "Usage:"
	@echo "  make build                Build for current host ($(HOST_TARGET))"
	@echo "  make build-all            Build for all buildable targets on this host"
	@echo "  make build-<target>       Build for a specific target"
	@echo "  make release              Build + package all targets into dist/"
	@echo "  make release-<target>     Build + package a specific target"
	@echo "  make check-deps           Verify prerequisites are installed"
	@echo "  make clean                Remove all build artifacts"
	@echo "  make clean-dist           Remove dist/ packages only"
	@echo "  make list-targets         List all supported targets"
	@echo "  make help                 Show this help"
	@echo ""
	@echo "Host: $(UNAME_S) $(UNAME_M) ($(HOST_TARGET))"

# ──────────────────────────────────────────────
# Build for host platform
# ──────────────────────────────────────────────
build:
	cargo build $(CARGO_FLAGS)

# ──────────────────────────────────────────────
# Per-target build rules
# ──────────────────────────────────────────────
define make_build_target
.PHONY: build-$(1)
build-$(1):
	@echo "━━━ Building $(BINARY_NAME) for $(1) ━━━"
	$$(call select_build_cmd,$(1)) $(CARGO_FLAGS) --target $(1)
	@echo "✓ Built: target/$(1)/release/$(BINARY_NAME)$$(call binary_ext,$(1))"
endef

$(foreach target,$(ALL_TARGETS),$(eval $(call make_build_target,$(target))))

# ──────────────────────────────────────────────
# Build all buildable targets for current host
# ──────────────────────────────────────────────
build-all: $(addprefix build-,$(BUILDABLE_TARGETS))
	@echo ""
	@echo "Skipped targets (not buildable on $(UNAME_S)): $(filter-out $(BUILDABLE_TARGETS),$(ALL_TARGETS))"

# ──────────────────────────────────────────────
# Per-target release packaging
# ──────────────────────────────────────────────
define make_release_target
.PHONY: release-$(1)
release-$(1): build-$(1)
	@mkdir -p $(DIST_DIR)
	@STAGING_DIR=$$$$(mktemp -d) && \
	cp target/$(1)/release/$(BINARY_NAME)$$(call binary_ext,$(1)) $$$$STAGING_DIR/ && \
	if [ "$$(call archive_ext,$(1))" = "zip" ]; then \
		(cd $$$$STAGING_DIR && zip -q $(CURDIR)/$(DIST_DIR)/$(BINARY_NAME)-v$(VERSION)-$(1).zip $(BINARY_NAME)$$(call binary_ext,$(1))); \
	else \
		tar -czf $(DIST_DIR)/$(BINARY_NAME)-v$(VERSION)-$(1).tar.gz -C $$$$STAGING_DIR $(BINARY_NAME)$$(call binary_ext,$(1)); \
	fi && \
	rm -rf $$$$STAGING_DIR
	@echo "✓ Packaged: $(DIST_DIR)/$(BINARY_NAME)-v$(VERSION)-$(1).$$(call archive_ext,$(1))"
endef

$(foreach target,$(ALL_TARGETS),$(eval $(call make_release_target,$(target))))

# ──────────────────────────────────────────────
# Release all buildable targets for current host
# ──────────────────────────────────────────────
release: $(addprefix release-,$(BUILDABLE_TARGETS))
	@echo ""
	@echo "━━━ Release packages ━━━"
	@ls -lh $(DIST_DIR)/$(BINARY_NAME)-v$(VERSION)-*
	@echo ""
	@echo "Checksums:"
	@cd $(DIST_DIR) && shasum -a 256 $(BINARY_NAME)-v$(VERSION)-* | tee $(BINARY_NAME)-v$(VERSION)-checksums.sha256

# ──────────────────────────────────────────────
# List targets
# ──────────────────────────────────────────────
list-targets:
	@echo "Supported targets:"
	@echo ""
	@echo "  Linux:"
	@$(foreach t,$(LINUX_TARGETS),echo "    $(t)";)
	@echo ""
	@echo "  macOS:"
	@$(foreach t,$(MACOS_TARGETS),echo "    $(t)";)
	@echo ""
	@echo "  Windows:"
	@$(foreach t,$(WINDOWS_TARGETS),echo "    $(t)";)
	@echo ""
	@echo "Host target: $(HOST_TARGET)"
	@echo "Buildable:   $(BUILDABLE_TARGETS)"

# ──────────────────────────────────────────────
# Check prerequisites
# ──────────────────────────────────────────────
check-deps:
	@echo "Checking prerequisites..."
	@echo ""
	@printf "  cargo:          " && (command -v cargo          >/dev/null 2>&1 && cargo --version || echo "NOT FOUND — install from https://rustup.rs")
	@printf "  rustup:         " && (command -v rustup         >/dev/null 2>&1 && rustup --version 2>/dev/null | head -1 || echo "NOT FOUND — install from https://rustup.rs")
	@printf "  cargo-zigbuild: " && (command -v cargo-zigbuild >/dev/null 2>&1 && cargo-zigbuild --version || echo "NOT FOUND — install with: cargo install cargo-zigbuild")
	@printf "  zig:            " && (command -v zig            >/dev/null 2>&1 && zig version || echo "NOT FOUND — install with: brew install zig (macOS) or see https://ziglang.org/download/")
	@printf "  tar:            " && (command -v tar            >/dev/null 2>&1 && echo "ok" || echo "NOT FOUND")
	@printf "  zip:            " && (command -v zip            >/dev/null 2>&1 && echo "ok" || echo "NOT FOUND — required for Windows .zip packages")
	@echo ""
	@echo "Installed Rust targets:"
	@rustup target list --installed 2>/dev/null || echo "  (rustup not available)"
	@echo ""
	@echo "To add a target:          rustup target add <target>"
	@echo "To install cargo-zigbuild: cargo install cargo-zigbuild"
	@echo "To install zig:            brew install zig  (macOS)"

# ──────────────────────────────────────────────
# Clean
# ──────────────────────────────────────────────
clean:
	cargo clean
	rm -rf $(DIST_DIR)

clean-dist:
	rm -rf $(DIST_DIR)
