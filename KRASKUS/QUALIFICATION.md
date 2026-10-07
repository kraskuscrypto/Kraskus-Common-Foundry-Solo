# Qualification record

**Status: Dev / experimental. Not qualified for production (Main).**

**Qualified source candidate (remote prover): `ef3ccfedb9755f178602e1126bea8871994da59e`** (2026-10-07). CI run [37664130558](https://github.com/kraskuscrypto/Kraskus-Common-Foundry-Solo/actions/runs/37664130558): `cargo fmt --check` and clippy `-D warnings` clean; tests node 35, prover 10, wire 7 all pass; mainnet launch identity OK; two independent builds byte-identical and equal to an independent local build:

| Binary | SHA-256 |
|---|---|
| `cmfd-node` (`1.0.8+kraskus-solo.1`) | `532de83f0cdd8d27ab07889cd42e13a444420ee97309482df9cc662c58dbc52e` |
| `kraskus-cmfd-prover` (`0.1.0+kraskus-solo`) | `a6b3abd18bcdd974682c0770bd84b5406ac7ea9e344750ea49f754358d2c7bff` |

Source only: no release and no published binaries. **Next gate: 4c** (real GPU self-test).

| # | Gate | Status | Evidence |
|---|---|---|---|
| 1 | Reproducible build | **PASS** | CI run [37631297910](https://github.com/kraskuscrypto/Kraskus-Common-Foundry-Solo/actions/runs/37631297910) on `d4d525a`: two independent GitHub builds gave `cmfd-node` sha256 `f204163249f76ae03e19d10fa219b4377a171f6be2630c46965cb1537d44c453`; an independent local Docker build of the same commit gave the identical hash (2026-10-07). Earlier commit `d0e0f97`: two local builds identical (`3600d68d…5746`). |
| 2 | Upstream mainnet launch identity | **PASS** | `mainnet-launch-info`: network id `88296bc3…2f62`, plan digest `21337265…04af`, beacon round 32747812, source commit = built commit. `cmfd-launch fetch` (official) verified the launch beacon for this binary. |
| 3 | Node, RPC, wallet behave as official | **Partial** | Mainnet profile, offline smoke test (official v1.0.8 package layout, official model bank, throwaway wallet): node opened, RPC answered, P2P bound. A synced-node comparison with the official binary is pending. |
| 4 | Endpoint states | **Partial** | `starting` → `prover_unavailable` (reason: "ProductionV4 pool proof worker is missing from the package", retried every 5 s), endpoint **not listening**, so no jobs. SIGINT → orderly stop, `stopped`. `ready` with the official workers on a GPU host is pending. |
| 4b | Remote prover, fail closed | **PASS (no-GPU host)** | Unit/protocol tests: wire 7, prover service 10, node remote client and readiness 9 (2026-10-07). Real processes on loopback (official v1.0.8 package layout, official model bank, no GPU, no fixed proving data): mutual TLS 1.3 pairing works and the capability reaches the node's status file; model bank verified (6.44 GB), the 7 missing fixed files are named; worker pins match; **not ready, endpoint closed**. Unknown node certificate: refused by the prover. Wrong prover pin: refused by the node. Both: endpoint closed. |
| 4c | Remote prover, GPU self-test | **Pending (next gate)** | On a 24 GB-class NVIDIA host: install and verify the official 61.2 GB proving data, start the official workers, run the node-generated self-test, confirm the official worker accepts the easiest-target test template, proof ≤ 30 s, node verifies the proof, Solo Prover Ready true, port 29445 opens only then. If the worker rejects the template, stop and report before any redesign. |
| 4d | Official miner through the endpoint | **Pending** | `cmfd-miner` connects via 29445, receives jobs, FW/s reported, winning-proof path exercised, endpoint closes immediately on prover failure, pairing/unpairing without node restart. |
| 5 | Block production | **Pending** | DevNet end to end in the test suite (`solo_endpoint_mines_a_devnet_block_to_the_node_wallet`): a block was mined through the solo configuration by the upstream pool client, accepted, and paid to the node wallet, with no ledger written. **The mainnet block-production test is required before any Main release.** |

**Tests:** `cargo test -p cmfd-node --features production-mainnet --bin cmfd-node` gives 35 passed (22 upstream, 13 Kraskus); the wire and prover crates give 17 passed. Clippy with `-D warnings` and `cargo fmt --check` are clean.
