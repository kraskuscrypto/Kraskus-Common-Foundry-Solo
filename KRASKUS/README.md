# Kraskus Common Foundry Solo

**Status: Dev / experimental. Not qualified for mainnet production use.**

This repository is the official Common Foundry source, [Common-Foundry-1/CommonFoundry](https://github.com/Common-Foundry-1/CommonFoundry) (MIT), at release **v1.0.8**, commit `3aa5369512f47d0b3c49a49a54a0395e71e0c000`, with its full upstream history. On top of it sits one small, separately reviewable Kraskus change. That change lets `cmfd-node run` serve the official pool protocol to the official `cmfd-miner` for **solo mining to the operator's own wallet**, while P2P, the loopback RPC, the wallet and the explorer keep running.

It exists only because v1.0.8 has no official solo mode for `cmfd-miner`: mainnet `pool-serve` requires payout settlement and stops the RPC. We asked upstream for an official mode in [Common-Foundry-1/CommonFoundry#19](https://github.com/Common-Foundry-1/CommonFoundry/issues/19). **When upstream ships one, this repository is retired and Kraskus uses the official signed binary.**

"Common Foundry" is the upstream project's name. This is a Kraskus build of their MIT-licensed node and implies no endorsement.

## What the Kraskus change does

All Kraskus code is in [`crates/cmfd-node/src/kraskus_solo.rs`](../crates/cmfd-node/src/kraskus_solo.rs), plus a short hook in `crates/cmfd-node/src/main.rs`: 25 added lines and one changed line, the `--version` string. See [PATCH.md](PATCH.md) for the reviewed diff.

- **New options on `run`, all off by default:**
  - `--solo-pool-bind <private address>`, `--solo-pool-certificate`, `--solo-pool-private-key` (from upstream `pool-certificate`)
  - `--solo-pool-miner <64-hex>`: optional; defaults to the node wallet
  - `--solo-pool-replay-worker`, `--solo-pool-proof-worker`, `--solo-pool-scratch`: the official ProductionV4 workers
  - `--solo-pool-status-file`, `--solo-pool-retry-seconds`
  - optionally `--solo-pool-dashboard-bind`, `--solo-pool-dashboard-assets`, `--solo-pool-public-url`: the upstream read-only dashboard and its `/api/v1/pool` worker statistics, on loopback
- **It runs the unmodified upstream `spawn_pool_server`, configured for one solo operator:**
  - The coinbase pays the operator's address.
  - No payout ledger on disk, no PPLNS, no payout transactions, no fee.
  - The share target equals the network target, so only block-winning nonces are submitted, then fully replayed and proven by the official workers before the block is accepted and relayed.
- **No prover, no jobs.** The endpoint listens only after the official replay and proof workers have started and reported ready. If they are missing or fail, which happens when the GPU or the proving data is missing, the node keeps running with RPC, P2P, wallet and explorer, but no miner can connect. The status file reports `prover_unavailable` with the reason, and the node retries.
- **`--version` reports `1.0.8+kraskus-solo.1`.**

**Not changed:** consensus, the PoW algorithm, block assembly, proof generation and verification, the wire protocol (TLS 1.3 with an exact certificate pin, protocol v2), the mainnet launch identity, the `pool-serve` command, and every other upstream path. Use the official `cmfd-miner` unchanged:

```
cmfd-miner pool --pool 'cmfd+tls://<node LAN IP>:<port>?pin=<certificate sha256>' ...
```

## Requirements for solo proving

A winning nonce must be fully replayed and turned into a ProductionV4 proof within the block time:

- **Proving data:** the official proving data, about 61 GB including the 6.4 GB model bank, on fast storage. Kraskus does not mirror it until upstream confirms redistribution rights.
- **GPU:** an NVIDIA GPU with 24 GB or more of memory is recommended. Upstream measured a 13.73 GiB peak, and 74 s end to end on a 16 GB card, against a 60 s block target. **A 12 GB card is not enough.**

## Build, verify, release

- **Build:** [BUILD.md](BUILD.md) covers the reproducible build in a pinned toolchain image, including the upstream mainnet launch-identity check.
- **Verify a release:** [VERIFY.md](VERIFY.md) covers the Ed25519 signature on `SHA256SUMS`. The public key is [`kraskus-solo-release.pub`](kraskus-solo-release.pub).
- **Release workflow:** [`.github/workflows/kraskus-solo-release.yml`](../.github/workflows/kraskus-solo-release.yml). The signing key is a protected environment secret held by the Kraskus owner. It is never in this repository and never on a developer machine.

## Qualification before any production (Main) use

1. Reproducible build: two independent builds produce the same SHA-256.
2. Mainnet launch-identity check passes (automated in the build).
3. Node sync, RPC, wallet and explorer behave exactly as the official binary.
4. Endpoint states verified: starting, prover unavailable (no jobs), ready.
5. **Block-production test:** a real block found by the official miner through this endpoint, proven, accepted by the network, and paying the operator's address.

Until all of these pass, releases are marked pre-release and are for Dev/experimental use only.

## Licence

Upstream MIT ([LICENSE](../LICENSE), [THIRD_PARTY_NOTICES.md](../THIRD_PARTY_NOTICES.md)). The Kraskus additions are MIT under the same licence.
