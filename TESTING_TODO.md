# Testing Improvements

## Done

- **Full-text snapshots instead of MD5s**: Replaced MD5-hashed snapshot tests with full-text insta snapshots of the generated bundled code. When output changes, `cargo insta review` shows the exact diff instead of two opaque hashes.
- **Configurable proc-macro-srv toolchain in tests**: Added `CARGO_EQUIP_TEST_PROC_MACRO_SRV_TOOLCHAIN` env var to decouple the proc-macro-srv binary version from the build/udeps toolchain.

## TODO

### Conversion round-trip tests for `ra_proc_macro.rs`

Test `proc_macro2 -> ra_ap_tt -> proc_macro2` round-trips for known token streams without needing a proc-macro-srv binary. These would catch breakage in the type-mapping code (renamed types, changed representations, new enum variants) which was most of the work in API upgrades like 0.0.166 -> 0.0.288.

Specific cases to cover:
- All delimiter types (parenthesis, brace, bracket, none/invisible)
- Idents (including keywords and raw idents)
- All punct spacing variants
- Literals (integers, floats, strings, byte strings, chars)
- Nested groups / mixed token trees
- Empty token streams

### Expansion integration tests for `ProcMacroExpander`

Feed a known proc macro dylib (e.g., proconio-derive) through `ProcMacroExpander` and snapshot the expanded tokens in isolation. This tests the RA protocol layer without running the full bundling pipeline.

### Separate proc macro expansion from bundling

The `bundle()` function in `lib.rs` combines dependency resolution, source rewriting, proc macro expansion, minification, and output checking in one pass. Extracting proc macro expansion behind a function/trait boundary would allow:
- Testing bundling with a fake expander (deterministic, no proc-macro-srv needed)
- Testing expansion separately with real proc macros
- Verifying just the expansion layer on RA upgrades without running the full pipeline

### Pin proc-macro-srv version in CI

Use the `CARGO_EQUIP_TEST_PROC_MACRO_SRV_TOOLCHAIN` env var in CI to pin a specific nightly, ensuring snapshot reproducibility across environments. When upgrading RA deps, update the pinned nightly and snapshots together.

### Compile-test the snapshot outputs

The bundling pipeline already runs `cargo check` on the output during tests. Consider also verifying that the snapshot `.snap` files themselves contain compilable Rust code, as a guard against snapshot staleness.
