# Testing Improvements

## Done

- **Full-text snapshots instead of MD5s**: Replaced MD5-hashed snapshot tests with full-text insta snapshots of the generated bundled code. When output changes, `cargo insta review` shows the exact diff instead of two opaque hashes.
- **Configurable proc-macro-srv toolchain in tests**: Added `CARGO_EQUIP_TEST_PROC_MACRO_SRV_TOOLCHAIN` env var to decouple the proc-macro-srv binary version from the build/udeps toolchain.
- **Conversion round-trip tests for `ra_proc_macro.rs`**: Test `proc_macro2 -> ra_ap_tt -> proc_macro2` round-trips for known token streams. Tests only construct `proc_macro2` types, so they don't need updating when `ra_ap_tt` types change — breakage surfaces as compile errors in the conversion functions, not the tests.
- **Expansion integration tests for `ProcMacroExpander`**: Build proconio-derive dylib, spawn `ProcMacroExpander`, and test attr expansion (`fastout`), macro listing, and unknown macro handling in isolation from the full bundling pipeline.

## TODO

### Separate proc macro expansion from bundling

The `bundle()` function in `lib.rs` combines dependency resolution, source rewriting, proc macro expansion, minification, and output checking in one pass. Extracting proc macro expansion behind a function/trait boundary would allow:
- Testing bundling with a fake expander (deterministic, no proc-macro-srv needed)
- Testing expansion separately with real proc macros
- Verifying just the expansion layer on RA upgrades without running the full pipeline

### Pin proc-macro-srv version in CI

Use the `CARGO_EQUIP_TEST_PROC_MACRO_SRV_TOOLCHAIN` env var in CI to pin a specific nightly, ensuring snapshot reproducibility across environments. When upgrading RA deps, update the pinned nightly and snapshots together.

### Compile-test the snapshot outputs

The bundling pipeline already runs `cargo check` on the output during tests. Consider also verifying that the snapshot `.snap` files themselves contain compilable Rust code, as a guard against snapshot staleness.

### Expand snapshot test coverage for syn 2.x upgrade

Coverage analysis (`cargo llvm-cov --html`) shows the existing snapshot tests exercise more of `rust.rs` than expected — the bundled library crates contain enough `#[cfg]`, `#[derive]`, `#[doc]`, and `#[warn/deny]` attributes to hit most `parse_meta` call sites. Key findings:

**Well-covered by existing snapshots:**
- `resolve_cfgs` — `proceed()` hit 14.4k times, `#[cfg(feature)]` (76 hits), `#[cfg(test)]` (18 hits), both remove-item and remove-attr branches exercised
- `translate_crate_path` — `visit_path` 14.3k hits, `visit_item_use` 268 hits
- `process_extern_crate_in_bin` — `macro_use` detection covered (6 hits)
- `allow_missing_docs` — `parse_meta` for warn/deny/forbid 243 hits, `missing_docs` replacement 4 hits
- `erase_docs` — 19 hits (snapshot + unit tests)

**Gaps worth filling with new test bins:**
- `#[cfg_attr(cargo_equip, cargo_equip::skip)]` detection (0 hits — no test uses the skip attribute)
- `crate::` path rewriting in library code (0 hits — test crates use `$crate::` or relative paths)
- Nested inline `mod` blocks in bin code (0 hits for `insert_prelude_for_main_crate` mod handling)
- `#[cfg(cargo_equip)]` predicate (0 hits)

**Blind spot — macro-generated visitors:** The `impl_visits!` macro blocks generate ~30 `visit_*` methods each, but llvm-cov collapses them into a single source line (e.g., 14.4k aggregate hits for `resolve_cfgs` visitors). Rare AST node types like `ExprTryBlock`, `ExprYield`, `ItemTraitAlias`, `ItemMacro2` are almost certainly never exercised, but this is invisible in the report. Behavioral regressions in these visitors would not be caught. Expanding the macros into explicit methods would give per-method visibility, but the methods all delegate to the same `proceed()` function, so the practical risk is low.

These are characterization tests: capture current behavior before the upgrade, then verify it doesn't change.

### Extract attribute-matching helpers in `rust.rs`

The `Attribute::parse_meta()` + match pattern repeats 9 times in `rust.rs`. This is the API that changes most in syn 2.x (`parse_meta()` is removed, `NestedMeta` is gone, `Meta::List` contains a `TokenStream` instead of `Punctuated<NestedMeta>`).

Extracting helpers like `is_macro_use(attr) -> bool`, `derive_names(attr) -> Vec<String>`, `is_doc(attr) -> bool` would:
- Make each helper independently unit-testable
- Isolate the syn API surface so the upgrade touches helpers only, not call sites
- Reduce the blast radius of the `parse_meta` removal

High-value pre-upgrade refactoring: directly isolates the syn API surface that changes most.

### Extract `CodeEdit` transformations into pure functions

Methods like `resolve_cfgs`, `process_extern_crate_in_bin`, `erase_docs` mutate `CodeEdit` internal state, making them testable only through the full pipeline. Extracting core logic into `fn(&str, ...) -> Result<String>` functions would allow direct unit testing of each transformation in isolation. This is the "Sprout Method" / "Extract and Override" pattern from *Working Effectively with Legacy Code*.

High-value pre-upgrade refactoring: cfg resolution and path rewriting are the most complex logic in `rust.rs`.

### Test coverage reporting

Add coverage reporting via `cargo-llvm-cov` or `cargo-tarpaulin`. The snapshot tests call `cargo_equip::run()` as a library function (not a subprocess), so cargo-equip's own code is instrumented normally — no special setup needed beyond what works for unit tests.

Caveats:
- cargo-equip spawns subprocesses (`cargo build`, `cargo check`, `cargo udeps`, `rust-analyzer-proc-macro-srv`) which won't be instrumented, but that's expected — we care about covering cargo-equip's logic, not cargo's.
- The existing `env::remove_var("RUSTFLAGS")` workaround in `tests/snapshots.rs` prevents coverage flags (e.g. `-C instrument-coverage`) from leaking into the `cargo check` calls that cargo-equip runs on bundled output. This should keep working as-is.
