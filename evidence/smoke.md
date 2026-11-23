# OpenObserve 0.6.2 component smoke

Tested source `d33df89d5b02be121a59803286f03f77656e2037`. The shared build script came from `provider-workflows` at `e4f30a669d9d537f2e52bbf69120bf07805ac49c`.

The build used Linux x86_64, Rust and Cargo 1.98.1, `wasm32-unknown-unknown` target, and wasm-tools 1.259.0. The source lockfile pins `wasmtime` 49.0.6 and `url` 2.5.9.

Commands ran in `/tmp/openobserve-prep-worker.HqVahl/provider` with `CARGO_HOME=/tmp/openobserve-prep-worker.HqVahl/cargo` and writable `TMPDIR=/tmp/openobserve-prep-worker.HqVahl/tmp`. Times are UTC on 2026-11-23.

| Command | Time | Result |
| --- | --- | --- |
| `env CARGO_BUILD_JOBS=1 CARGO_TARGET_DIR=/tmp/openobserve-prep-worker.HqVahl/target-wasm ../provider-workflows/build.sh` | 14:17:08–14:18:27 | Built `openobserve-provider.wasm`. |
| `sha256sum --check --strict openobserve-provider.wasm.sha256` | 14:18:27–14:18:28 | Passed. Wasm SHA256 `4640c33d5eb9ea1126d00ac16596fdbbdc8193e964325c7fe550cc6019cb5582`. |
| `wasm-tools component wit openobserve-provider.wasm >../component.wit` | 14:18:28 | Decoded WIT matched the published v0.6.1 component byte for byte. |
| `cargo test --locked --workspace --no-run` followed by `cargo test --locked --workspace` | 14:22:31–14:22:33 | Both commands passed with `DEKOPON_PROVIDER_COMPONENT=/tmp/openobserve-prep-worker.HqVahl/provider/openobserve-provider.wasm` and `CARGO_BUILD_JOBS=4`. All 66 tests passed, with 0 failed and 0 ignored. The suite included real-component ABI and manifest conformance and synthetic response replay. |
