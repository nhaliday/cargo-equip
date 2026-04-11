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

### Test coverage reporting

Add coverage reporting via `cargo-llvm-cov` or `cargo-tarpaulin`. The snapshot tests call `cargo_equip::run()` as a library function (not a subprocess), so cargo-equip's own code is instrumented normally — no special setup needed beyond what works for unit tests.

Caveats:
- cargo-equip spawns subprocesses (`cargo build`, `cargo check`, `cargo udeps`, `rust-analyzer-proc-macro-srv`) which won't be instrumented, but that's expected — we care about covering cargo-equip's logic, not cargo's.
- The existing `env::remove_var("RUSTFLAGS")` workaround in `tests/snapshots.rs` prevents coverage flags (e.g. `-C instrument-coverage`) from leaking into the `cargo check` calls that cargo-equip runs on bundled output. This should keep working as-is.
