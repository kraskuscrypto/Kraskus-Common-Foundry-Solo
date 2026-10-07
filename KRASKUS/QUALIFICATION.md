# Qualification record

**Status: Dev / experimental. Not qualified for production (Main).**

| # | Gate | Status | Evidence |
|---|---|---|---|
| 1 | Reproducible build | **PASS** | CI run [37631297910](https://github.com/kraskuscrypto/Kraskus-Common-Foundry-Solo/actions/runs/37631297910) on `d4d525a`: two independent GitHub builds gave `cmfd-node` sha256 `f204163249f76ae03e19d10fa219b4377a171f6be2630c46965cb1537d44c453`; an independent local Docker build of the same commit gave the identical hash (2026-10-07). Earlier commit `d0e0f97`: two local builds identical (`3600d68d…5746`). |
| 2 | Upstream mainnet launch identity | **PASS** | `mainnet-launch-info`: network id `88296bc3…2f62`, plan digest `21337265…04af`, beacon round 32747812, source commit = built commit. `cmfd-launch fetch` (official) verified the launch beacon for this binary. |
| 3 | Node, RPC, wallet behave as official | **Partial** | Mainnet profile, offline smoke test (official v1.0.8 package layout, official model bank, throwaway wallet): node opened, RPC answered, P2P bound. A synced-node comparison with the official binary is pending. |
| 4 | Endpoint states | **Partial** | `starting` → `prover_unavailable` (reason: "ProductionV4 pool proof worker is missing from the package", retried every 5 s), endpoint **not listening**, so no jobs. SIGINT → orderly stop, `stopped`. `ready` with the official workers on a GPU host is pending. |
| 5 | Block production | **Pending** | DevNet end to end in the test suite (`solo_endpoint_mines_a_devnet_block_to_the_node_wallet`): a block was mined through the solo configuration by the upstream pool client, accepted, and paid to the node wallet, with no ledger written. **The mainnet block-production test is required before any Main release.** |

**Tests:** `cargo test -p cmfd-node --features production-mainnet --bin cmfd-node` gave 26 passed: 22 upstream and 4 Kraskus. Clippy with `-D warnings` and `cargo fmt --check` are clean.
