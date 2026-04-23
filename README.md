# fix-engine

Generic fix engine for applying pattern-based and LLM-assisted code migration fixes.

Takes [Konveyor](https://www.konveyor.io/) analysis output and applies automated fixes to your codebase — deterministic pattern-based renames, attribute removals, import rewrites, and LLM-assisted structural transformations.

## Quick Start

```bash
# Build
cargo build --release

# Apply fixes (default behavior — writes to disk)
fix-engine fix ./my-project -i analysis.yaml

# Preview changes as a diff without writing
fix-engine fix ./my-project -i analysis.yaml --dry-run

# With fix strategies
fix-engine fix ./my-project -i analysis.yaml \
  --strategies rules/fix-strategies.json \
  --strategies output/fix-strategies.json
```

## Installation

```bash
cargo install --path .
```

### Cross-compilation

See [CROSS-COMPILATION.md](CROSS-COMPILATION.md) for building Linux binaries on macOS.

## Usage

### Basic

```bash
# Apply all fixes to the project
fix-engine fix ./project -i konveyor-output.yaml

# Preview what would change (unified diff)
fix-engine fix ./project -i konveyor-output.yaml --dry-run
```

### With LLM-assisted fixes

```bash
# Use local goose CLI for complex fixes
fix-engine fix ./project -i output.yaml --llm-provider goose

# Use a remote OpenAI-compatible endpoint
fix-engine fix ./project -i output.yaml \
  --llm-provider openai \
  --llm-endpoint https://api.openai.com/v1/chat/completions
```

### With fix strategies

Strategy files are JSON maps of rule ID to fix strategy. They can come from two sources:

- **Hand-written** — bundled alongside rule YAML files
- **Auto-generated** — produced by `semver-analyzer`'s `konveyor` command

Pass multiple files with `--strategies`; later files override earlier ones on key conflicts:

```bash
fix-engine fix ./project -i output.yaml \
  --strategies rules/fix-strategies.json \
  --strategies output/fix-strategies.json
```

### CI/CD integration

```bash
# JSON output for machine parsing
fix-engine fix ./project -i output.yaml --dry-run --output-format json

# Quiet mode (errors + summary only)
fix-engine fix ./project -i output.yaml -q

# Disable colors for log files
fix-engine fix ./project -i output.yaml --color never
```

### Shell completions

```bash
# Generate and install (zsh example)
fix-engine completions zsh > ~/.zsh/completions/_fix-engine

# Other shells: bash, fish, elvish, powershell
fix-engine completions bash > /etc/bash_completion.d/fix-engine
```

## CLI Reference

```
fix-engine fix [OPTIONS] --input <INPUT> <PROJECT>
```

| Flag | Short | Description |
|------|-------|-------------|
| `<PROJECT>` | | Path to the project to fix (positional, required) |
| `--input` | `-i` | Path to Konveyor analysis output — YAML or JSON (required) |
| `--dry-run` | | Preview changes as a unified diff without writing to disk |
| `--llm-provider` | | LLM provider: `goose` (local CLI) or `openai` (remote endpoint) |
| `--llm-endpoint` | | LLM endpoint URL (required when `--llm-provider=openai`) |
| `--strategies` | | Fix strategies JSON file(s) — can be repeated, later overrides earlier |
| `--log-dir` | | Directory to save goose prompts/responses for debugging |
| `--verbose` | `-v` | Show detailed output |
| `--quiet` | `-q` | Suppress progress, show only errors and summary |
| `--output-format` | | Output format: `text` (default) or `json` |
| `--color` | | Color mode: `auto` (default), `always`, or `never` |

### Environment variables

| Variable | Description |
|----------|-------------|
| `RUST_LOG` | Control log verbosity (e.g., `RUST_LOG=debug`). Default: `info`. Log output is routed through the progress display so it won't clobber active spinners. |

## Architecture

The project is a Cargo workspace with four crates:

| Crate | Description |
|-------|-------------|
| `fix-engine-cli` | CLI binary — argument parsing, progress reporting, orchestration |
| `fix-engine-core` | Core types — text edits, fix plans, strategies, fix result |
| `fix-engine` | Engine — pattern-based planning/applying, goose/LLM clients |
| `fix-engine-js-fix` | JS/TS language provider — prop removal, import dedup, lockfile management |

### Fix pipeline

1. **Load** — Parse Konveyor analysis output (YAML or JSON)
2. **Plan** — Map each violation to a fix strategy (pattern, LLM, or manual)
3. **Apply patterns** — Deterministic renames, removals, import rewrites
4. **Apply LLM fixes** — Send complex structural changes to goose or OpenAI
5. **Report** — Show results, manual review items, and summary

## License

Apache-2.0
