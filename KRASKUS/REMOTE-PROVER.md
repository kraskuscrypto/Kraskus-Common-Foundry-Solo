# Remote LAN GPU prover: boundary and protocol (DESIGN, not implemented)

**Status:** design for owner review, 2026-10-07. Nothing in this document is built yet.

**Goal:** the Common Foundry node, for example on 5tratumOS, stays GPU-independent. The replay and proof work needed for a winning block runs on a separate GPU machine on the LAN.

## 1. Where the boundary is (from the upstream source)

Upstream already isolates the GPU work behind one Rust trait. The pool server never touches the GPU directly:

```rust
// crates/cmfd-node/src/pool.rs:329
pub trait ProductionV4PoolShareVerifier: Send + Sync {
    fn evaluate(&self, template: &BlockTemplate, nonce: u64, share_target: [u8; 32])
        -> Result<ProductionV4PoolShareEvaluation /* work_digest, Option<BlockProof> */, PoolError>;
}
```

- **Local implementation today:** `ProductionV4PersistentPoolVerifier` (`production_v4_pool.rs`). It writes `coefficients.bin` and `template.json` into a scratch directory, then drives the official workers over their line protocol:
  1. the replay worker in search mode, then in full mode;
  2. the proof worker;
  3. it reads back the two final activations and `transparent-proof.bin`.
- **The large data stays between the two workers:** the full-replay intermediates and the 61 GB proving data.
- **The node trusts nothing the verifier returns.** `evaluate_production_v4_pool_share` (`pool.rs:5279`) checks that the work digest and the proof agree. The block is then admitted only through the node's own consensus verifier, the same path as any block from a peer.

**Decision:** cut at the trait, not at the file system.

- On the GPU host, a Kraskus **prover service** wraps the **unmodified** upstream `ProductionV4PersistentPoolVerifier` and the official workers.
- On the node, a Kraskus `RemoteProverVerifier` implements the same trait by calling that service.
- No shared directory, no remote shell, and no worker line protocol crosses the network.

## 2. What crosses the boundary (complete list)

| Direction | Data | Sensitive? | Bound |
|---|---|---|---|
| node → prover | `BlockTemplate`: challenge (height, parent, target, timestamp, network id), coinbase (outputs to the payout **address**), transactions (public mempool data) | No. Public block data. **No private keys, no wallet file, no passphrase.** | ≤ the consensus maximum block size (16 MiB) |
| node → prover | `nonce` (u64), `share_target` (32 bytes) | No | fixed |
| prover → node | `work_digest` (32 bytes) | No | fixed |
| prover → node | optional `BlockProof::V4Candidate` (serialized transparent proof) | No | ≤ `PRODUCTION_V4_MAX_PROOF_BYTES` |
| prover → node | capability and health report (section 5) | No | ≤ 16 KiB |

**Never crosses:**
- wallet keys, seed or passphrase;
- node RPC access;
- chain database;
- the miner endpoint's TLS private key;
- the full-replay intermediates (they stay on the prover host and are deleted after each attempt, as upstream does);
- the proving data.

## 3. Authority (who decides what)

| Responsibility | Owner |
|---|---|
| Chain state, wallet, block template, coinbase destination, block assembly, submission and relay | **Node only** |
| Consensus validity of any proof or block | **Node only**, through the unchanged upstream verifier. A bad or malicious prover cannot get an invalid block accepted. |
| GPU replay and proof generation for a submitted winning nonce | Prover, with the unmodified upstream verifier and official workers |
| Miner connections, jobs, shares | Node only. Miners never talk to the prover. |

**Worst case for a faulty prover:** it can only cause a **missed block**, by returning "not a winner" or an error. It cannot cause a consensus fault. That risk is why the readiness and self-test rules in section 5 exist.

**No consensus logic in Kraskus code:** the wrapper serializes and deserializes upstream types with upstream codecs, and calls upstream functions on both ends. It reimplements nothing.

## 4. Transport and authentication

- **Encryption and authentication:** TLS 1.3 (`rustls`, as upstream) with **mutual exact certificate pinning.**
  - The node pins the prover's certificate SHA-256.
  - The prover pins the node's client-certificate SHA-256.
  - Both certificates come from the upstream `pool-certificate` generator, one per install. Private keys never leave their host.
- **LAN-only:** both sides refuse non-private addresses, with the same rule as upstream's `validate_private_address`: loopback, RFC 1918, or IPv6 ULA. There are no public-client options.
- **Framing:** length-prefixed JSON frames, the same style as the upstream pool protocol, with byte bounds per message type. One request in flight at a time, and every request has a deadline.
- **Session start:** `hello` from the node, carrying protocol version, network id and consensus fingerprint. The prover answers with `hello_ack` and its capability report. Any mismatch closes the connection.
- **Messages:**

| Message | Direction | Purpose |
|---|---|---|
| `hello` / `hello_ack` | node → prover → node | Versions, network id, consensus fingerprint, capabilities |
| `status` | node → prover | Current capability and health (section 5); polled every 10 s |
| `evaluate` | node → prover | `{request_id, template, nonce, share_target}` → `{work_digest, proof?, timings}`; deadline 120 s |
| `self_test` | node → prover | Prove a test template (section 5) → `{work_digest, proof, timings}` |
| `error` | either | Typed error; the node treats it as fail-closed |

- **Pairing UX:**
  - The prover prints its URL `cmfd-prover+tls://<LAN IP>:29460?pin=<sha256>`, which the user pastes into the node app.
  - The node app shows its client fingerprint, which the user adds to the prover's allow list.
  - No passwords and no shared secrets, which keeps the V3 rule that the wallet password is the only user credential.

## 5. Capability, health and fail-closed readiness

The prover reports:
- **Identity:** prover version; upstream commit; SHA-256 of the replay worker, the proof worker and `libcudart`.
- **GPU:** name, UUID, compute capability, **total and free VRAM**, driver version. Read with NVML or `nvidia-smi`; no settings are changed.
- **Proving data:** model bank SHA-256 verified (yes/no); fixed record identity; codeword and tree files present with exact sizes and verified against the upstream manifest (yes/no, and when).
- **Workers:** started and READY (yes/no).
- **Last self-test:** time, result, and the replay, full and proof timings.

**Self-test:**
1. The node builds a test template from the current tip with the easiest possible target (all `0xff`), so every nonce is a "winner" and the prover always runs the full replay and proof. The template is never submitted, and it could never be a valid block.
2. The node checks the returned proof with the upstream `verify_forgematrix_v4_transparent_proof`, the same proof verifier used for block admission. The exact call path will be confirmed during implementation.
3. It runs at prover connect, every 6 hours, and after any prover error.

**Solo Prover Ready** is true only if **all** of these hold:
1. connected and mutually authenticated, with `hello` matching;
2. GPU detected with VRAM ≥ 16 GiB (24 GiB+ recommended; upstream's peak replay allocation is 13.73 GiB);
3. proving data present and verified;
4. both workers READY;
5. last self-test passed, is less than 24 h old, and its total time is within the proof budget (default 30 s against the 60 s block target; owner decision);
6. last `status` reply less than 30 s old.

**Fail closed:**
- If any check fails, the node supervisor **stops the miner endpoint listener.** Miners are disconnected, no jobs go out, and the status file shows `prover_unavailable` with the failed check.
- The endpoint restarts only after all checks pass again.
- An `evaluate` error rejects that share (upstream behaviour) and immediately marks the prover unhealthy.
- The node never shows "Solo Ready" from cached or optimistic state.

## 6. Prover host requirements

- **GPU:** NVIDIA GPU of a compute capability the official workers support. **24 GB+ VRAM recommended**; 16 GB fits (13.73 GiB peak) but upstream measured 74 s end to end on an RTX 5070 Ti, above the 60 s block target. An RTX 5090 proves in about 6 s. **12 GB cards are not supported.**
- **Driver:** NVIDIA driver and CUDA 12 runtime. The official `libcudart.so.12` ships in the upstream runtime package.
- **Storage:** about **61.2 GB** of proving data (6.4 GB model bank, three 17.18 GB row-major codewords, three 1.07 GB Merkle trees) on NVMe or SSD, plus scratch space for one attempt. Downloaded by upstream's own `PREPARE-V4-INPUTS.sh` from the official sources. **Kraskus does not mirror it** until upstream confirms redistribution rights (upstream #19).
- **Software:** the signed upstream v1.0.8 runtime package (workers `production-v4/cmfd-v4-replay` and `production-v4/real_bank0_relations`), plus the Kraskus prover service binary from this repository, built reproducibly and signed like the node.
- **OS:** Linux x86_64, or Windows with WSL2 (upstream already supports WSL workers).

## 7. What is implemented where

| Piece | Location | Notes |
|---|---|---|
| `RemoteProverVerifier`, its supervision and the `--solo-pool-remote-prover …` options | `crates/cmfd-node/src/kraskus_solo.rs` | Node side. Implements the upstream trait. |
| `kraskus-cmfd-prover` service | new crate `crates/kraskus-cmfd-prover` | Links `cmfd_node` and runs the unmodified `ProductionV4PersistentPoolVerifier`. |
| Shared wire types and bounds | `kraskus-cmfd-prover` (library) | Upstream types serialized with upstream codecs. |

**Tests before any Dev binary:**
- loopback node ↔ prover, with a fake verifier, on DevNet;
- mismatched pin, network or fingerprint is refused;
- a public address is refused;
- prover kill, hang or slow response → endpoint stops, then recovers;
- self-test failure keeps the endpoint closed;
- oversize frames are rejected;
- the real GPU self-test on a 24 GB-class host.
