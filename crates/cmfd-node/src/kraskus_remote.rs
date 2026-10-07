//! Node side of the Kraskus remote LAN prover (Kraskus-Common-Foundry-Solo; NOT upstream).
//!
//! - `RemoteProver`: the mutually pinned TLS 1.3 client (`kraskus-cmfd-prover-wire`).
//! - `RemoteVerifier`: implements the upstream `ProductionV4PoolShareVerifier`, so the
//!   unmodified upstream pool server calls it exactly like its local verifier. Every
//!   returned proof still passes the node's own checks in `pool.rs` and its consensus
//!   verifier before a block is accepted.
//! - `assess`: the fail-closed Solo Prover Ready decision.
//! - `self_test`: the node asks the prover to prove a never-submittable test template
//!   (easiest possible target) and verifies the proof itself with upstream
//!   `ConsensusPowVerifier::verify_evaluation`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cmfd_consensus::{BlockChallenge, BlockProof};
use cmfd_node::BlockTemplate;
use cmfd_node::pool::{PoolError, ProductionV4PoolShareEvaluation, ProductionV4PoolShareVerifier};
use kraskus_cmfd_prover_wire::{
    self as wire, Capability, ClientStream, Request, Response, WireError,
};
use serde_json::{Value, json};

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub const STATUS_TIMEOUT: Duration = Duration::from_secs(15);
pub const EVALUATE_TIMEOUT: Duration = Duration::from_secs(120);
/// Owner decision: half of the 60 s block target.
pub const PROOF_BUDGET_SECONDS: f64 = 30.0;
pub const MIN_VRAM_MIB: u64 = 16 * 1024;
pub const REPORT_MAX_AGE: Duration = Duration::from_secs(30);
pub const SELF_TEST_MAX_AGE: Duration = Duration::from_secs(24 * 3600);
pub const SELF_TEST_INTERVAL: Duration = Duration::from_secs(6 * 3600);

pub struct RemoteProver {
    pub address: SocketAddr,
    pin: [u8; 32],
    network_id: [u8; 32],
    tls: Arc<wire::rustls::ClientConfig>,
    connection: Mutex<Option<ClientStream>>,
    next_request: AtomicU64,
    /// Set by any evaluation failure; cleared only by a passing self-test.
    failed: Mutex<Option<String>>,
    in_flight_since: Mutex<Option<Instant>>,
}

impl std::fmt::Debug for RemoteProver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteProver")
            .field("address", &self.address)
            .finish_non_exhaustive()
    }
}

impl RemoteProver {
    pub fn new(
        url: &str,
        network_id: [u8; 32],
        client_certificate_der: Vec<u8>,
        client_key_der: Vec<u8>,
    ) -> Result<Self, WireError> {
        let (address, pin) = wire::parse_url(url)?;
        Ok(Self {
            address,
            pin,
            network_id,
            tls: wire::client_config(pin, client_certificate_der, client_key_der)?,
            connection: Mutex::new(None),
            next_request: AtomicU64::new(1),
            failed: Mutex::new(None),
            in_flight_since: Mutex::new(None),
        })
    }

    pub fn url(&self) -> String {
        wire::format_url(self.address, self.pin)
    }

    fn open(&self) -> Result<(ClientStream, Capability), WireError> {
        let mut stream = wire::connect(self.address, Arc::clone(&self.tls), CONNECT_TIMEOUT)?;
        wire::write_message(
            &mut stream,
            &Request::Hello {
                protocol: wire::PROTOCOL.to_owned(),
                network_id: wire::hex32(&self.network_id),
                node_version: crate::kraskus_solo::VERSION.to_owned(),
            },
            &[],
            0,
        )?;
        match wire::read_message::<_, Response>(&mut stream, 0)?.0 {
            Response::HelloAck {
                protocol,
                network_id,
                capability,
            } if protocol == wire::PROTOCOL && network_id == wire::hex32(&self.network_id) => {
                Ok((stream, capability))
            }
            Response::Error { code, message } => Err(WireError::new(
                "prover_refused",
                format!("{code}: {message}"),
            )),
            _ => Err(WireError::new(
                "protocol",
                "prover hello does not match this protocol or network",
            )),
        }
    }

    /// One request/response on the shared connection; any error drops the
    /// connection so the next call reconnects and re-authenticates.
    fn call(
        &self,
        request: &Request,
        blobs: &[Vec<u8>],
        timeout: Duration,
        wait: bool,
    ) -> Result<(Response, Vec<Vec<u8>>), WireError> {
        let mut slot = if wait {
            self.connection
                .lock()
                .map_err(|_| WireError::new("internal", "connection lock poisoned"))?
        } else {
            match self.connection.try_lock() {
                Ok(slot) => slot,
                Err(_) => return Err(WireError::new("busy", "an evaluation is in progress")),
            }
        };
        if slot.is_none() {
            *slot = Some(self.open()?.0);
        }
        let stream = slot.as_mut().expect("connection present");
        let result = wire::set_read_timeout(stream, timeout)
            .and_then(|_| wire::write_message(stream, request, blobs, wire::MAX_REQUEST_BLOB_BYTES))
            .and_then(|_| wire::read_message::<_, Response>(stream, wire::MAX_RESPONSE_BLOB_BYTES));
        if result.is_err() {
            *slot = None;
        }
        result
    }

    pub fn status(&self) -> Result<Capability, WireError> {
        match self.call(&Request::Status, &[], STATUS_TIMEOUT, false)?.0 {
            Response::Status { capability } => Ok(capability),
            Response::Error { code, message } => {
                Err(WireError::new("prover_error", format!("{code}: {message}")))
            }
            _ => Err(WireError::new("protocol", "unexpected status reply")),
        }
    }

    pub fn evaluation_in_flight(&self) -> Option<Duration> {
        self.in_flight_since
            .lock()
            .ok()
            .and_then(|s| s.map(|t| t.elapsed()))
    }

    pub fn failure(&self) -> Option<String> {
        self.failed.lock().ok().and_then(|f| f.clone())
    }

    pub fn clear_failure(&self) {
        if let Ok(mut f) = self.failed.lock() {
            *f = None;
        }
    }

    fn record_failure(&self, error: &WireError) {
        if let Ok(mut f) = self.failed.lock() {
            *f = Some(error.to_string());
        }
    }

    pub fn evaluate_template(
        &self,
        template: &BlockTemplate,
        nonce: u64,
        share_target: [u8; 32],
    ) -> Result<ProductionV4PoolShareEvaluation, WireError> {
        if let Ok(mut s) = self.in_flight_since.lock() {
            *s = Some(Instant::now());
        }
        let result = self.evaluate_inner(template, nonce, share_target);
        if let Ok(mut s) = self.in_flight_since.lock() {
            *s = None;
        }
        if let Err(error) = &result {
            self.record_failure(error);
        }
        result
    }

    fn evaluate_inner(
        &self,
        template: &BlockTemplate,
        nonce: u64,
        share_target: [u8; 32],
    ) -> Result<ProductionV4PoolShareEvaluation, WireError> {
        let request_id = self.next_request.fetch_add(1, Ordering::AcqRel);
        let blobs = wire::encode_transactions(&template.transactions)?;
        let request = Request::Evaluate {
            request_id,
            challenge: template.challenge,
            coinbase: template.coinbase.clone(),
            total_fees_burned: template.total_fees_burned,
            transactions: blobs.len(),
            nonce,
            share_target: wire::hex32(&share_target),
        };
        let (response, mut proof_blobs) = self.call(&request, &blobs, EVALUATE_TIMEOUT, true)?;
        match response {
            Response::Evaluation {
                request_id: answered,
                work_digest,
                proof,
                ..
            } => {
                if answered != request_id {
                    return Err(WireError::new(
                        "protocol",
                        "prover answered another request",
                    ));
                }
                let work_digest = wire::parse_hex32(&work_digest)?;
                let chain_proof = match (proof, proof_blobs.len()) {
                    (false, 0) => None,
                    (true, 1) => {
                        let proof = wire::decode_proof(&proof_blobs.remove(0), self.network_id)?;
                        if proof.work_digest() != work_digest {
                            return Err(WireError::new(
                                "proof_mismatch",
                                "proof work digest differs from the reply",
                            ));
                        }
                        Some(proof)
                    }
                    _ => {
                        return Err(WireError::new(
                            "protocol",
                            "proof flag and attachments disagree",
                        ));
                    }
                };
                Ok(ProductionV4PoolShareEvaluation {
                    work_digest,
                    chain_proof,
                })
            }
            Response::Error { code, message } => {
                Err(WireError::new("prover_error", format!("{code}: {message}")))
            }
            _ => Err(WireError::new("protocol", "unexpected evaluation reply")),
        }
    }
}

/// The upstream trait over the network. Errors fail the share (upstream
/// behaviour) and mark the prover failed until a new self-test passes.
#[derive(Debug, Clone)]
pub struct RemoteVerifier(pub Arc<RemoteProver>);

impl ProductionV4PoolShareVerifier for RemoteVerifier {
    fn evaluate(
        &self,
        template: &BlockTemplate,
        nonce: u64,
        share_target: [u8; 32],
    ) -> Result<ProductionV4PoolShareEvaluation, PoolError> {
        self.0
            .evaluate_template(template, nonce, share_target)
            .map_err(|e| PoolError::ProductionV4Replay(format!("remote prover: {e}")))
    }
}

// ------------------------------------------------------------- self-test

#[derive(Debug, Clone, PartialEq)]
pub struct SelfTest {
    pub at_unix: u64,
    pub at: Instant,
    pub ok: bool,
    pub total_seconds: f64,
    pub error: Option<String>,
}

/// Verifies a proof for the test challenge with the node's own upstream code.
pub type ProofCheck = dyn Fn(&BlockChallenge, &BlockProof) -> Result<(), String> + Send + Sync;

/// The test template is the node's own next template with the easiest
/// possible target: every nonce "wins", so the prover must run the full
/// replay and proof. It can never be a valid block and is never submitted.
pub fn self_test(
    prover: &RemoteProver,
    mut template: BlockTemplate,
    nonce: u64,
    check: &ProofCheck,
    now_unix: u64,
) -> SelfTest {
    template.challenge.target = [0xff; 32];
    let started = Instant::now();
    let outcome = prover
        .evaluate_template(&template, nonce, [0xff; 32])
        .map_err(|e| e.to_string())
        .and_then(|evaluation| match evaluation.chain_proof {
            None => Err("prover returned no proof for the test template".to_owned()),
            Some(proof) if proof.work_digest() != evaluation.work_digest => {
                Err("proof work digest differs from the reply".to_owned())
            }
            Some(proof) => check(&template.challenge, &proof),
        });
    let total_seconds = started.elapsed().as_secs_f64();
    let (ok, error) = match outcome {
        Ok(()) if total_seconds <= PROOF_BUDGET_SECONDS => (true, None),
        Ok(()) => (
            false,
            Some(format!(
                "proof took {total_seconds:.1} s; the limit is {PROOF_BUDGET_SECONDS:.0} s"
            )),
        ),
        Err(e) => (false, Some(e)),
    };
    if ok {
        prover.clear_failure();
    }
    SelfTest {
        at_unix: now_unix,
        at: Instant::now(),
        ok,
        total_seconds,
        error,
    }
}

/// Builds the node's own upstream ProductionV4 verifier from the packaged
/// model bank and fixed record (both authenticated by upstream code).
#[cfg(feature = "production-v4")]
pub fn upstream_proof_check(
    artifacts: &cmfd_node::ProductionV4VerifierArtifacts,
    network_id: [u8; 32],
) -> Result<Box<ProofCheck>, String> {
    let record: cmfd_consensus::ForgeMatrixV4FixedArtifactRecordV1 =
        serde_json::from_slice(&std::fs::read(&artifacts.fixed_record).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let bank = std::fs::File::open(&artifacts.bank).map_err(|e| e.to_string())?;
    let verifier = cmfd_consensus::ConsensusPowVerifier::v4_candidate_for_network(
        network_id,
        record,
        std::io::BufReader::with_capacity(64 * 1024 * 1024, bank),
    )
    .map_err(|e| e.to_string())?;
    Ok(Box::new(move |challenge, proof| {
        verifier
            .verify_evaluation(challenge, proof)
            .map_err(|e| e.to_string())
    }))
}

// ------------------------------------------------------------ assessment

#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    pub ready: bool,
    pub reasons: Vec<String>,
    /// The `prover` object of the KRASKUS_CMFD_SOLO_STATUS_V1 status file.
    pub report: Value,
}

/// Solo Prover Ready only if every check passes; anything missing fails closed.
#[allow(clippy::too_many_arguments)]
pub fn assess(
    url: &str,
    capability: Option<&Capability>,
    reported_at: Option<Instant>,
    status_error: Option<&str>,
    self_test: Option<&SelfTest>,
    failure: Option<&str>,
    network_id: [u8; 32],
    now_unix: u64,
) -> Assessment {
    let mut reasons = Vec::new();
    let fresh = reported_at.is_some_and(|t| t.elapsed() <= REPORT_MAX_AGE);
    let connected = capability.is_some() && fresh;
    if !connected {
        reasons.push(status_error.map_or_else(
            || "block prover not connected".to_owned(),
            |e| format!("block prover unreachable: {e}"),
        ));
    }
    let cap = capability.cloned().unwrap_or_default();
    if capability.is_some() && cap.network_id != wire::hex32(&network_id) {
        reasons.push("prover is for another network".to_owned());
    }
    let vram = cap.gpu.as_ref().and_then(|g| g.vram_total_mib);
    if vram.is_none_or(|v| v < MIN_VRAM_MIB) {
        reasons.push(match vram {
            None => "prover GPU memory not reported".to_owned(),
            Some(v) => format!(
                "prover GPU has {:.1} GiB; at least 16 GiB is required",
                v as f64 / 1024.0
            ),
        });
    }
    if !cap.proving_data.verified {
        reasons.push(
            cap.proving_data
                .detail
                .clone()
                .unwrap_or_else(|| "proving data not verified".to_owned()),
        );
    }
    if !(cap.workers.ready && cap.workers.pins_ok) {
        reasons.push(
            cap.workers
                .detail
                .clone()
                .unwrap_or_else(|| "official workers not ready".to_owned()),
        );
    }
    let test_ok = self_test.is_some_and(|t| t.ok && t.at.elapsed() <= SELF_TEST_MAX_AGE);
    if !test_ok {
        reasons.push(
            self_test
                .and_then(|t| t.error.clone())
                .unwrap_or_else(|| "no recent successful proof self-test".to_owned()),
        );
    }
    if let Some(f) = failure {
        reasons.push(format!("last evaluation failed: {f}"));
    }
    let report = json!({
        "mode": "remote",
        "address": url,
        "connected": connected,
        "reported_unix": if connected { json!(now_unix) } else { Value::Null },
        "error": reasons.first(),
        "gpu": cap.gpu.as_ref().map(|g| json!({
            "name": g.name, "uuid": g.uuid, "compute_capability": g.compute_capability,
            "vram_total_mib": g.vram_total_mib, "vram_free_mib": g.vram_free_mib,
            "driver_version": g.driver_version,
        })),
        "proving_data": {
            "verified": cap.proving_data.verified, "bytes": cap.proving_data.bytes,
            "expected_bytes": cap.proving_data.expected_bytes,
            "checked_unix": cap.proving_data.checked_unix, "detail": cap.proving_data.detail,
        },
        "workers_ready": cap.workers.ready && cap.workers.pins_ok,
        "workers": {"replay_sha256": cap.workers.replay_sha256, "proof_sha256": cap.workers.proof_sha256,
                    "detail": cap.workers.detail},
        "prover_version": cap.prover_version,
        "last_self_test": self_test.map(|t| json!({
            "ok": t.ok, "at_unix": t.at_unix, "total_seconds": t.total_seconds, "error": t.error,
        })),
        "proof_budget_seconds": PROOF_BUDGET_SECONDS,
    });
    Assessment {
        ready: reasons.is_empty(),
        reasons,
        report,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cmfd_consensus::{
        Coinbase, ForgeMatrixV4CandidateProof, PRODUCTION_V4_TESTNET_NETWORK_ID as NET,
    };
    use kraskus_cmfd_prover_wire::{DataStatus, GpuInfo, WorkerStatus};
    use std::net::TcpListener;
    use std::thread;

    type Reply = fn(u64) -> (Response, Vec<Vec<u8>>);

    fn identity() -> (Vec<u8>, Vec<u8>) {
        let generated =
            rcgen::generate_simple_self_signed(vec![wire::SERVER_NAME.to_owned()]).unwrap();
        (
            generated.cert.der().to_vec(),
            generated.signing_key.serialize_der(),
        )
    }

    fn capability(vram: u64) -> Capability {
        Capability {
            prover_version: "test".to_owned(),
            upstream_commit: "3aa5369".to_owned(),
            network_id: wire::hex32(&NET),
            gpu: Some(GpuInfo {
                name: "NVIDIA GeForce RTX 5090".to_owned(),
                vram_total_mib: Some(vram),
                ..GpuInfo::default()
            }),
            proving_data: DataStatus {
                verified: true,
                ..DataStatus::default()
            },
            workers: WorkerStatus {
                ready: true,
                pins_ok: true,
                ..WorkerStatus::default()
            },
            ..Capability::default()
        }
    }

    fn proof(work: [u8; 32]) -> BlockProof {
        BlockProof::V4Candidate(Box::new(ForgeMatrixV4CandidateProof {
            algorithm_version: 1,
            proof_version: 1,
            nonce: 5,
            proof_system_digest: [1; 32],
            model_manifest_digest: [2; 32],
            challenge_digest: [3; 32],
            final_activation_digest: [4; 32],
            work_digest: work,
            transparent_proof: b"test-proof".to_vec(),
        }))
    }

    fn template() -> BlockTemplate {
        BlockTemplate {
            challenge: BlockChallenge {
                network_id: NET,
                previous_block: [1; 32],
                transaction_root: [2; 32],
                height: 3,
                timestamp: 4,
                target: [0; 32],
            },
            coinbase: Coinbase {
                height: 3,
                outputs: vec![],
            },
            transactions: vec![],
            total_fees_burned: 0,
        }
    }

    /// A scripted prover: answers hello, status, and each evaluate with `reply`.
    fn fake_prover(
        hello_network: [u8; 32],
        reply: Reply,
    ) -> (RemoteProver, thread::JoinHandle<()>) {
        let (server_cert, server_key) = identity();
        let (node_cert, node_key) = identity();
        let tls = wire::server_config(
            server_cert.clone(),
            server_key,
            vec![wire::certificate_sha256(&node_cert)],
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let Ok((socket, _)) = listener.accept() else {
                return;
            };
            let Ok(mut stream) = wire::accept(socket, tls, Duration::from_secs(5)) else {
                return;
            };
            let _ = wire::read_message::<_, Request>(&mut stream, 0);
            let _ = wire::write_message(
                &mut stream,
                &Response::HelloAck {
                    protocol: wire::PROTOCOL.to_owned(),
                    network_id: wire::hex32(&hello_network),
                    capability: capability(32607),
                },
                &[],
                0,
            );
            while let Ok((request, _)) =
                wire::read_message::<_, Request>(&mut stream, wire::MAX_REQUEST_BLOB_BYTES)
            {
                let (response, blobs) = match request {
                    Request::Status => (
                        Response::Status {
                            capability: capability(32607),
                        },
                        vec![],
                    ),
                    Request::Evaluate { request_id, .. } => reply(request_id),
                    Request::Hello { .. } => return,
                };
                if wire::write_message(
                    &mut stream,
                    &response,
                    &blobs,
                    wire::MAX_RESPONSE_BLOB_BYTES,
                )
                .is_err()
                {
                    return;
                }
            }
        });
        let url = wire::format_url(address, wire::certificate_sha256(&server_cert));
        (
            RemoteProver::new(&url, NET, node_cert, node_key).unwrap(),
            handle,
        )
    }

    fn evaluation(request_id: u64, work: [u8; 32], with_proof: bool) -> (Response, Vec<Vec<u8>>) {
        let blobs = if with_proof {
            vec![wire::encode_proof(&proof(work), NET).unwrap()]
        } else {
            vec![]
        };
        (
            Response::Evaluation {
                request_id,
                work_digest: wire::hex32(&work),
                proof: with_proof,
                seconds: 0.1,
            },
            blobs,
        )
    }

    fn good(id: u64) -> (Response, Vec<Vec<u8>>) {
        evaluation(id, [9; 32], true)
    }

    fn no_proof(id: u64) -> (Response, Vec<Vec<u8>>) {
        evaluation(id, [9; 32], false)
    }

    fn wrong_id(id: u64) -> (Response, Vec<Vec<u8>>) {
        evaluation(id + 1, [9; 32], false)
    }

    fn flag_without_blob(id: u64) -> (Response, Vec<Vec<u8>>) {
        (evaluation(id, [9; 32], true).0, vec![])
    }

    fn blob_without_flag(id: u64) -> (Response, Vec<Vec<u8>>) {
        (
            evaluation(id, [9; 32], false).0,
            evaluation(id, [9; 32], true).1,
        )
    }

    fn digest_mismatch(id: u64) -> (Response, Vec<Vec<u8>>) {
        let (mut response, blobs) = evaluation(id, [9; 32], true);
        if let Response::Evaluation { work_digest, .. } = &mut response {
            *work_digest = wire::hex32(&[7; 32]);
        }
        (response, blobs)
    }

    fn refused(_id: u64) -> (Response, Vec<Vec<u8>>) {
        (
            Response::Error {
                code: "not_ready".to_owned(),
                message: "workers".to_owned(),
            },
            vec![],
        )
    }

    #[test]
    fn remote_evaluation_returns_the_upstream_shape() {
        let (prover, _server) = fake_prover(NET, good);
        assert_eq!(
            prover.status().unwrap().gpu.unwrap().vram_total_mib,
            Some(32607)
        );
        let result = RemoteVerifier(Arc::new(prover))
            .evaluate(&template(), 5, [0; 32])
            .unwrap();
        assert_eq!(result.work_digest, [9; 32]);
        assert_eq!(result.chain_proof.unwrap().work_digest(), [9; 32]);
    }

    #[test]
    fn mismatched_replies_fail_closed_and_mark_the_prover_failed() {
        for reply in [
            wrong_id as Reply,
            flag_without_blob,
            blob_without_flag,
            digest_mismatch,
            refused,
        ] {
            let (prover, _server) = fake_prover(NET, reply);
            assert!(prover.evaluate_template(&template(), 5, [0; 32]).is_err());
            assert!(
                prover.failure().is_some(),
                "an evaluation failure must be recorded"
            );
        }
    }

    #[test]
    fn prover_for_another_network_is_refused() {
        let (prover, _server) = fake_prover([0x33; 32], no_proof);
        assert_eq!(prover.status().unwrap_err().code, "protocol");
    }

    #[test]
    fn wrong_prover_pin_is_refused() {
        let (prover, _server) = fake_prover(NET, no_proof);
        let (cert, key) = identity();
        let wrong =
            RemoteProver::new(&wire::format_url(prover.address, [1; 32]), NET, cert, key).unwrap();
        assert!(wrong.status().is_err());
    }

    #[test]
    fn unreachable_or_public_provers_are_errors_not_hangs() {
        let (cert, key) = identity();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let prover =
            RemoteProver::new(&wire::format_url(address, [1; 32]), NET, cert, key).unwrap();
        assert!(prover.status().is_err());
        let public = format!("cmfd-prover+tls://8.8.8.8:1?pin={}", "a".repeat(64));
        assert!(RemoteProver::new(&public, NET, vec![], vec![]).is_err());
    }

    #[test]
    fn self_test_is_verified_by_the_node_and_held_to_the_budget() {
        let (prover, _server) = fake_prover(NET, good);
        let accept = |challenge: &BlockChallenge, proof: &BlockProof| -> Result<(), String> {
            assert_eq!(
                challenge.target, [0xff; 32],
                "test template uses the easiest target"
            );
            assert_eq!(proof.work_digest(), [9; 32]);
            Ok(())
        };
        let passed = self_test(&prover, template(), 5, &accept, 100);
        assert!(passed.ok, "{:?}", passed.error);
        assert!(passed.total_seconds <= PROOF_BUDGET_SECONDS);

        let reject = |_: &BlockChallenge, _: &BlockProof| -> Result<(), String> {
            Err("transparent proof does not verify".to_owned())
        };
        let failed = self_test(&prover, template(), 6, &reject, 101);
        assert!(!failed.ok && failed.error.unwrap().contains("does not verify"));
    }

    #[test]
    fn self_test_without_a_proof_fails() {
        let (prover, _server) = fake_prover(NET, no_proof);
        let accept = |_: &BlockChallenge, _: &BlockProof| -> Result<(), String> { Ok(()) };
        let t = self_test(&prover, template(), 5, &accept, 100);
        assert!(!t.ok && t.error.unwrap().contains("no proof"));
    }

    fn passing_test() -> SelfTest {
        SelfTest {
            at_unix: 1,
            at: Instant::now(),
            ok: true,
            total_seconds: 7.0,
            error: None,
        }
    }

    fn assessed(
        cap: Option<&Capability>,
        reported: Option<Instant>,
        test: Option<&SelfTest>,
        failure: Option<&str>,
    ) -> Assessment {
        assess(
            "cmfd-prover+tls://192.168.1.50:29460?pin=00",
            cap,
            reported,
            None,
            test,
            failure,
            NET,
            1,
        )
    }

    #[test]
    fn solo_prover_ready_requires_every_check() {
        let good = capability(32607);
        let now = Some(Instant::now());
        let a = assessed(Some(&good), now, Some(&passing_test()), None);
        assert!(a.ready, "{:?}", a.reasons);
        assert_eq!(a.report["workers_ready"], true);
        assert_eq!(a.report["last_self_test"]["ok"], true);
        assert_eq!(a.report["gpu"]["vram_total_mib"], 32607);

        let small = capability(12227);
        let mut no_data = capability(32607);
        no_data.proving_data.verified = false;
        let mut no_workers = capability(32607);
        no_workers.workers.ready = false;
        let mut unpinned = capability(32607);
        unpinned.workers.pins_ok = false;
        let mut other = capability(32607);
        other.network_id = wire::hex32(&[0x33; 32]);
        let mut no_gpu = capability(32607);
        no_gpu.gpu = None;
        for cap in [&small, &no_data, &no_workers, &unpinned, &other, &no_gpu] {
            assert!(!assessed(Some(cap), now, Some(&passing_test()), None).ready);
        }
        let stale = Some(Instant::now() - REPORT_MAX_AGE - Duration::from_secs(1));
        assert!(
            !assessed(Some(&good), stale, Some(&passing_test()), None).ready,
            "stale report"
        );
        assert!(
            !assessed(None, now, Some(&passing_test()), None).ready,
            "never connected"
        );
        assert!(
            !assessed(Some(&good), now, None, None).ready,
            "no self-test"
        );
        let slow = SelfTest {
            ok: false,
            error: Some("proof took 74.3 s; the limit is 30 s".to_owned()),
            ..passing_test()
        };
        let a = assessed(Some(&good), now, Some(&slow), None);
        assert!(!a.ready && a.reasons.iter().any(|r| r.contains("30 s")));
        let old = SelfTest {
            at: Instant::now() - SELF_TEST_MAX_AGE - Duration::from_secs(1),
            ..passing_test()
        };
        assert!(
            !assessed(Some(&good), now, Some(&old), None).ready,
            "self-test older than 24 h"
        );
        assert!(
            !assessed(
                Some(&good),
                now,
                Some(&passing_test()),
                Some("worker exited")
            )
            .ready,
            "failed evaluation"
        );
    }
}
