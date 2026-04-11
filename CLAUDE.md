# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

cargo-equip is a Cargo subcommand that bundles code from multiple Rust library crates into a single `.rs` file for competitive programming submissions. It resolves dependencies, rewrites crate paths, expands procedural macros, and optionally minifies/formats the output.

## Build and test

The CI workflow references `nightly-2023-08-04` but is out-of-date. Use a recent stable or nightly toolchain instead:

```bash
# Build
cargo build

# Run all tests (must be single-threaded for test isolation)
cargo test --no-fail-fast -- --test-threads 1

# Run a single test
cargo test <test_name> -- --test-threads 1

# Lint
cargo clippy --all-targets -- -D warnings

# Format check
cargo fmt --all -- --check

# Update snapshots (uses insta)
INSTA_UPDATE=always cargo test -- --test-threads 1
cargo insta review
```

Use `--manifest-path Cargo.toml` if cargo resolves to the wrong workspace (e.g., tests/solutions).

### Environment variables

- `CARGO_EQUIP_TEST_NIGHTLY_TOOLCHAIN` — nightly toolchain for cargo-udeps (default: `"nightly"`)
- `CARGO_EQUIP_TEST_PROC_MACRO_SRV_TOOLCHAIN` — toolchain whose `rust-analyzer-proc-macro-srv` binary is used for proc macro expansion tests

## Architecture

**Entry point:** `main.rs` → `lib.rs::run()` → `lib.rs::bundle()`

**Core flow:** Parse CLI args → load cargo metadata → find unused deps via cargo-udeps → build dependency graph → rewrite code (path substitution, macro expansion, cfg resolution) → minify/format → verify with cargo check → emit

Key modules:

- **`rust.rs`** (~1800 lines) — The heart of code transformation. `CodeEdit` struct wraps syn AST and tracks text replacements. Handles extern crate rewriting, `#[macro_use]` expansion, `mod` inlining, cfg resolution, comment/doc stripping. Uses syn 1.x (cannot parse edition 2024 syntax like `const {}` blocks or `if let` chains).
- **`ra_proc_macro.rs`** — Proc macro expansion via rust-analyzer's `proc-macro-srv` binary. Converts between `proc_macro2` and `ra_ap_tt` token trees. The `ra_ap_*` crates are pinned to exact versions (`=0.0.288`) because the proc-macro-srv protocol must match.
- **`workspace.rs`** — Cargo metadata queries, edition handling, temporary workspace creation for `cargo check` validation.
- **`cargo_udeps.rs`** — Runs `cargo udeps` to determine which dependencies are actually used.
- **`toolchain.rs`** — Rustup/toolchain detection utilities.

## Testing

- **Snapshot tests** (`tests/snapshots.rs`): Full-text snapshots of bundled output using `insta`. Test fixtures are real Cargo projects in `tests/solutions/`.
- **Round-trip tests** (in `ra_proc_macro.rs`): `proc_macro2 → ra_ap_tt → proc_macro2` conversion fidelity.
- **Expansion integration tests** (in `ra_proc_macro.rs`): Build actual proc-macro dylibs and test expansion through proc-macro-srv.
- **Help snapshot** (`tests/help-snapshot.rs`): CLI help text snapshot.

## Key constraints

- The `ra_ap_*` dependency versions must match the `rust-analyzer-proc-macro-srv` binary version from the toolchain. Version mismatches cause protocol errors.
- `syn` 1.x is used throughout `rust.rs`. Upgrading to syn 2.x would be a large change affecting most of that file.
- Tests that invoke cargo-equip as a binary (snapshot tests) need `--test-threads 1` because they share filesystem state.
