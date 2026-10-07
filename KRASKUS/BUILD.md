# Reproducible build

Pins are listed in [build/PINS](build/PINS):

- **Upstream:** v1.0.8, commit `3aa5369`.
- **Toolchain image:** `rust:1.94.1-slim-bookworm`, pinned by digest. Rust 1.94.1 is the toolchain upstream CI uses for its mainnet node.

## Build

```bash
KRASKUS/build/build.sh ./out
```

**Requirements:** Docker and a clean checkout. The build refuses uncommitted tracked changes and a history that does not contain the upstream commit.

**What it does:**
1. Builds `cargo build --release --locked -p cmfd-node --features production-mainnet --bin cmfd-node` inside the pinned image. Paths are fixed and remapped, and `SOURCE_DATE_EPOCH` is the commit time.
2. Runs `cmfd-node mainnet-launch-info` and asserts:
   - the upstream mainnet network id `88296bc3…2f62`;
   - the launch-plan digest `21337265…04af`;
   - beacon round `32747812`;
   - the built commit.

   These are the same assertions as upstream's own mainnet node workflow.
3. Checks that `run --help` includes the solo options.
4. Writes these files to `out/`:
   - `cmfd-node`
   - `KRASKUS-PATCH.diff`: the code diff against upstream, excluding `KRASKUS/` docs and Kraskus workflows
   - `MAINNET-LAUNCH-INFO.json`
   - `VERSION.txt`
   - `BUILD-INFO.json`
   - `SHA256SUMS`

## Reproducibility check

CI builds the same commit twice in separate jobs and fails if the `cmfd-node` SHA-256 values differ. Anyone can check the result: run `KRASKUS/build/build.sh` on the release commit and compare `out/SHA256SUMS` with the release's `SHA256SUMS`.

## Tests

```bash
cargo test --locked -p cmfd-node --features production-mainnet --bin cmfd-node kraskus_solo
cargo clippy --locked -p cmfd-node --features production-mainnet --bin cmfd-node --tests -- -D warnings
```
