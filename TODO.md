# TODO

## Feature unification spuriously includes dev-dependencies

My library `algolib-rs` depends on `proptest` when the `testing` feature is on.
`cargo-equip` fails to bundle this because of a transitive dep of `proptest`,
whereas it shouldn't be bundling `proptest` or its deps in the first place.
