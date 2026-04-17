# Cross-Compilation Guide

This project uses `cargo-zigbuild` with Zig as the cross-linker to build binaries for Linux, macOS, and Windows from a single host machine. No containers or VMs are required.

## Prerequisites

### Rust toolchain

```bash
# Install Rust (if not already installed)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Add cross-compilation targets
rustup target add \
  x86_64-unknown-linux-gnu \
  x86_64-unknown-linux-musl \
  aarch64-unknown-linux-gnu \
  aarch64-unknown-linux-musl \
  x86_64-apple-darwin \
  aarch64-apple-darwin \
  x86_64-pc-windows-gnu
```

### Zig (version 0.14.x required)

Zig is used as the C cross-linker by `cargo-zigbuild`. **Version 0.14.x is required** — Zig 0.16+ has breaking changes in its `ar` implementation that cause build failures with C dependencies (ring, etc.).

```bash
# macOS (Homebrew)
brew install zig@0.14

# If zig 0.16 is already installed, switch to 0.14:
brew unlink zig
brew install zig@0.14
brew link zig@0.14 --force

# Linux — download from https://ziglang.org/download/
# Use the 0.14.1 release for your architecture.

# Windows — download from https://ziglang.org/download/
# Use the 0.14.1 release (zip archive, add to PATH).

# Verify version
zig version
# Expected: 0.14.1
```

### cargo-zigbuild

```bash
cargo install cargo-zigbuild

# Verify
cargo zigbuild --help
```

### Packaging tools (for `make release`)

- `tar` — included on macOS and Linux
- `zip` — `brew install zip` on macOS, `apt install zip` on Debian/Ubuntu

## TLS note

The `reqwest` dependency uses `rustls` (pure Rust TLS) instead of `native-tls` (OpenSSL) to enable cross-compilation without needing system OpenSSL headers for each target. This is configured in `crates/engine/Cargo.toml`.

## Usage

```bash
# Check all prerequisites
make check-deps

# Build for current host (plain cargo, no zig)
make build

# Build for a specific target
make build-x86_64-unknown-linux-gnu
make build-aarch64-unknown-linux-musl
make build-x86_64-pc-windows-gnu

# Build all targets buildable from this host
make build-all

# Build + package release archives (tar.gz / zip)
make release

# Package a single target
make release-aarch64-unknown-linux-gnu

# List all targets
make list-targets
```

## Supported targets

| Target | Build tool | Archive format |
|--------|-----------|----------------|
| `x86_64-unknown-linux-gnu` | cargo zigbuild | .tar.gz |
| `x86_64-unknown-linux-musl` | cargo zigbuild | .tar.gz |
| `aarch64-unknown-linux-gnu` | cargo zigbuild | .tar.gz |
| `aarch64-unknown-linux-musl` | cargo zigbuild | .tar.gz |
| `x86_64-apple-darwin` | cargo (native, macOS only) | .tar.gz |
| `aarch64-apple-darwin` | cargo (native, macOS only) | .tar.gz |
| `x86_64-pc-windows-gnu` | cargo zigbuild | .zip |
| `x86_64-pc-windows-msvc` | cargo (native, Windows only) | .zip |

## Platform constraints

- **macOS targets** can only be built on macOS (Apple SDK is not redistributable).
- **Windows MSVC** can only be built on Windows. Use `x86_64-pc-windows-gnu` for cross-compilation.
- **All Linux targets and Windows-GNU** can be built from any host via `cargo-zigbuild`.

## Release output

Release archives are written to `dist/`:

```
dist/
  fix-engine-cli-v0.0.1-x86_64-unknown-linux-gnu.tar.gz
  fix-engine-cli-v0.0.1-aarch64-unknown-linux-musl.tar.gz
  fix-engine-cli-v0.0.1-x86_64-pc-windows-gnu.zip
  fix-engine-cli-v0.0.1-checksums.sha256
  ...
```

## Troubleshooting

### `ar: error: unable to open ... No such file or directory`

You are likely using Zig 0.16+. Downgrade to Zig 0.14.x:

```bash
brew unlink zig
brew install zig@0.14
brew link zig@0.14 --force
```

Then clear the cached wrappers and rebuild:

```bash
rm -rf ~/Library/Caches/cargo-zigbuild/
make build-all
```

### Missing rustup target

```
error[E0463]: can't find crate for `std`
```

Add the target: `rustup target add <target>`

### macOS targets fail on Linux

macOS targets require the Apple SDK which is only available on macOS. Build on a macOS machine or use macOS CI runners.

### OpenSSL errors during cross-compilation

If you see errors about `openssl-sys` or `OPENSSL_DIR`, a dependency is using `native-tls` instead of `rustls`. Check that `reqwest` in `crates/engine/Cargo.toml` has `default-features = false` and uses the `rustls-tls` feature.
