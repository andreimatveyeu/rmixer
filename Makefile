# Makefile for rmixer
# Provides convenient build commands with proper environment setup

# Detect if running on NixOS
IS_NIXOS := $(shell test -d /nix/store && echo yes)

ifeq ($(IS_NIXOS),yes)
    # Find libjack2 pkg-config path in Nix store
    JACK_PC_PATH := $(shell find /nix/store -name "jack.pc" -path "*libjack2*" 2>/dev/null | head -1 | xargs dirname 2>/dev/null)
    
    # Fallback to jack2 if libjack2 not found
    ifeq ($(JACK_PC_PATH),)
        JACK_PC_PATH := $(shell find /nix/store -name "jack.pc" -path "*jack2*" 2>/dev/null | head -1 | xargs dirname 2>/dev/null)
    endif
    
    # Export PKG_CONFIG_PATH for NixOS
    export PKG_CONFIG_PATH := $(JACK_PC_PATH)
endif

.PHONY: all build release run clean check test help

# Default target
all: build

# Debug build
build:
ifeq ($(IS_NIXOS),yes)
	@echo "NixOS detected, using PKG_CONFIG_PATH=$(PKG_CONFIG_PATH)"
endif
	cargo build

# Release build
release:
ifeq ($(IS_NIXOS),yes)
	@echo "NixOS detected, using PKG_CONFIG_PATH=$(PKG_CONFIG_PATH)"
endif
	cargo build --release

# Run with example config
run: build
	./target/debug/rmixer --config config.example.yaml

# Run release with example config
run-release: release
	./target/release/rmixer --config config.example.yaml

# Clean build artifacts
clean:
	cargo clean

# Check code without building
check:
ifeq ($(IS_NIXOS),yes)
	@echo "NixOS detected, using PKG_CONFIG_PATH=$(PKG_CONFIG_PATH)"
endif
	cargo check

# Run tests
test:
ifeq ($(IS_NIXOS),yes)
	@echo "NixOS detected, using PKG_CONFIG_PATH=$(PKG_CONFIG_PATH)"
endif
	cargo test

# Show help
help:
	@echo "rmixer Makefile targets:"
	@echo "  build       - Build debug version"
	@echo "  release     - Build release version"
	@echo "  run         - Build and run with example config"
	@echo "  run-release - Build release and run with example config"
	@echo "  clean       - Remove build artifacts"
	@echo "  check       - Check code without building"
	@echo "  test        - Run tests"
	@echo "  help        - Show this help"
ifeq ($(IS_NIXOS),yes)
	@echo ""
	@echo "NixOS detected"
	@echo "JACK pkg-config path: $(JACK_PC_PATH)"
endif
