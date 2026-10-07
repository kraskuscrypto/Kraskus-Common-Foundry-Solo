# The Kraskus change, file by file

The base is upstream v1.0.8, commit `3aa5369512f47d0b3c49a49a54a0395e71e0c000`. It is kept with its full history; the Kraskus commits sit directly on top of it.

To see the exact code change:

```bash
git diff 3aa5369512f47d0b3c49a49a54a0395e71e0c000 HEAD -- . ':(exclude)KRASKUS' ':(exclude).github/workflows/kraskus-*'
```

Every release also ships this diff as `KRASKUS-PATCH.diff`, and its hash is listed in the signed `SHA256SUMS`.

| File | Change |
|---|---|
| `crates/cmfd-node/src/kraskus_solo.rs` | **New.** The `run` options, the solo configuration of the upstream pool server, the supervisor (prover pairing, heartbeat, self-test, open/close the endpoint), status and snapshot files, tests. |
| `crates/cmfd-node/src/kraskus_remote.rs` | **New.** Node-side prover client; `RemoteVerifier` implements the upstream `ProductionV4PoolShareVerifier`; node-side self-test verification; the fail-closed readiness assessment; tests. |
| `crates/kraskus-cmfd-prover-wire/` | **New crate.** Protocol messages, framing limits, TLS 1.3 with mutual exact pinning, private-address rules; transactions and proofs in the upstream consensus wire codec. |
| `crates/kraskus-cmfd-prover/` | **New crate.** The prover service: proving-data and worker-pin verification, GPU report, the unmodified upstream `ProductionV4PersistentPoolVerifier` with the official workers. |
| `crates/cmfd-node/src/main.rs` | `mod kraskus_remote; mod kraskus_solo;`; one flattened option group on `run`; spawn after the RPC starts; startup JSON key `kraskus_solo_pool`; supervision and orderly stop; `--version` reads `1.0.8+kraskus-solo.1`. |
| `crates/cmfd-node/Cargo.toml`, `Cargo.toml`, `Cargo.lock` | One path dependency on the wire crate; two workspace members; the matching lock entries. No new third-party crates. |
| `KRASKUS/**`, `.github/workflows/kraskus-solo-release.yml` | Documentation, reproducible build and the signed-release workflow. These are not compiled into the binary. |

## What the solo configuration sets

`solo_pool_config()` starts from the upstream `PoolServerConfig::devnet` defaults and sets:

| Field | Value | Effect |
|---|---|---|
| `block_destination` | `--solo-pool-miner`, or the node wallet | The coinbase pays the operator directly. |
| `share_target` | all zeros | `spawn_pool_server` uses the easier of this and the chain target, i.e. **exactly the network target**. |
| `payout_policy` | `None` | No payout transactions, so upstream's payout wallet check does not apply. |
| `pplns_policy` | `None` | No PPLNS, no operator fee. |
| `ledger_directory` | `None` | No payout ledger on disk. |
| `test_credit_atoms_per_share` | `0` | No share credits. |
| `allow_public_clients` | `false` | Miners must connect from private or loopback addresses. This is the upstream default. |
| `allow_address_only_payouts` | `false` | Upstream default. |

`production_v4_share_verifier` is `RemoteVerifier`. The prover runs the unmodified upstream `ProductionV4PersistentPoolVerifier` with the same worker commands upstream `pool-serve` uses.

## Why this is not a consensus change

All of these are the upstream code paths, unchanged:
- block templates (`build_mining_job`);
- the share and nonce checks;
- the full replay and proof of a winning nonce;
- block acceptance;
- P2P relay.

The binary is built with the upstream `production-mainnet` feature and passes the upstream mainnet launch-identity assertions (see [BUILD.md](BUILD.md)). It forms blocks that any official node accepts or rejects by the same rules.

## Upstream code paths reused

| Upstream file | Used for |
|---|---|
| `crates/cmfd-node/src/pool.rs` | `spawn_pool_server`, `PoolServerConfig`, `certificate_sha256` |
| `crates/cmfd-node/src/pool_dashboard.rs` | `spawn_pool_dashboard` (optional) |
| `crates/cmfd-node/src/production_v4_pool.rs` | `ProductionV4PersistentPoolVerifier` (prover side) |
| `crates/cmfd-consensus/src/pow.rs` | `ConsensusPowVerifier::v4_candidate_for_network`, `verify_evaluation` (node-side self-test check) |
| `crates/cmfd-consensus/src/wire.rs` | `encode_transaction`, `encode_forgematrix_proof` and their decoders |
