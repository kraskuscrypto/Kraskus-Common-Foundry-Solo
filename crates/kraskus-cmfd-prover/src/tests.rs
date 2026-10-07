use super::*;
use cmfd_consensus::{BlockChallenge, Coinbase};
use cmfd_node::pool::PoolError;
use kraskus_cmfd_prover_wire::{MAX_REQUEST_BLOB_BYTES, MAX_RESPONSE_BLOB_BYTES};
use std::sync::atomic::AtomicU64;

const NETWORK: [u8; 32] = [5; 32];

#[derive(Debug, Default)]
struct FakeVerifier {
    calls: AtomicU64,
    fail: bool,
}

impl ProductionV4PoolShareVerifier for FakeVerifier {
    fn evaluate(
        &self,
        template: &BlockTemplate,
        nonce: u64,
        share_target: [u8; 32],
    ) -> Result<ProductionV4PoolShareEvaluation, PoolError> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if self.fail {
            return Err(PoolError::ProductionV4Replay("worker exited".to_owned()));
        }
        assert_eq!(template.challenge.network_id, NETWORK);
        assert_eq!(share_target, [0xff; 32]);
        let mut digest = [0_u8; 32];
        digest[..8].copy_from_slice(&nonce.to_be_bytes());
        Ok(ProductionV4PoolShareEvaluation {
            work_digest: digest,
            chain_proof: None,
        })
    }
}

struct FakeGpu;

impl GpuProbe for FakeGpu {
    fn probe(&self) -> Result<GpuInfo, String> {
        Ok(GpuInfo {
            name: "NVIDIA GeForce RTX 5090".to_owned(),
            uuid: Some("GPU-test".to_owned()),
            compute_capability: Some("12.0".to_owned()),
            vram_total_mib: Some(32607),
            vram_free_mib: Some(30000),
            driver_version: Some("615.71.09".to_owned()),
        })
    }
}

fn prover() -> Arc<Prover> {
    Prover::new(
        ProverIdentity {
            network_id: NETWORK,
            upstream_commit: "3aa5369512f47d0b3c49a49a54a0395e71e0c000".to_owned(),
        },
        Box::new(FakeGpu),
    )
}

fn ready(prover: &Prover, verifier: Arc<dyn ProductionV4PoolShareVerifier>) {
    prover.set_data(DataStatus {
        verified: true,
        ..DataStatus::default()
    });
    prover.set_workers(WorkerStatus {
        pins_ok: true,
        ..WorkerStatus::default()
    });
    prover.install_verifier(verifier);
}

fn challenge(network_id: [u8; 32]) -> BlockChallenge {
    BlockChallenge {
        network_id,
        previous_block: [1; 32],
        transaction_root: [2; 32],
        height: 7,
        timestamp: 1_791_400_000,
        target: [0xff; 32],
    }
}

fn coinbase() -> Coinbase {
    Coinbase {
        height: 7,
        outputs: vec![],
    }
}

fn eval(
    prover: &Prover,
    network: [u8; 32],
    declared: usize,
) -> Result<(ProductionV4PoolShareEvaluation, f64), WireError> {
    prover.evaluate(
        challenge(network),
        coinbase(),
        0,
        declared,
        &[],
        42,
        &"ff".repeat(32),
    )
}

#[test]
fn evaluation_fails_closed_until_data_and_workers_are_ready() {
    let p = prover();
    assert_eq!(eval(&p, NETWORK, 0).unwrap_err().code, "not_ready");
    p.set_data(DataStatus {
        verified: true,
        ..DataStatus::default()
    });
    assert_eq!(eval(&p, NETWORK, 0).unwrap_err().code, "not_ready");
    p.set_workers(WorkerStatus {
        pins_ok: false,
        ..WorkerStatus::default()
    });
    p.install_verifier(Arc::new(FakeVerifier::default()));
    assert_eq!(
        eval(&p, NETWORK, 0).unwrap_err().code,
        "not_ready",
        "unpinned workers"
    );
    p.set_workers(WorkerStatus {
        pins_ok: true,
        ready: true,
        ..WorkerStatus::default()
    });
    let (evaluation, _) = eval(&p, NETWORK, 0).unwrap();
    assert_eq!(&evaluation.work_digest[..8], &42_u64.to_be_bytes());
    p.withdraw_verifier("replay worker exited".to_owned());
    assert_eq!(eval(&p, NETWORK, 0).unwrap_err().code, "not_ready");
    assert_eq!(
        p.capability().workers.detail.as_deref(),
        Some("replay worker exited")
    );
}

#[test]
fn wrong_network_and_mismatched_transactions_are_refused() {
    let p = prover();
    ready(&p, Arc::new(FakeVerifier::default()));
    assert_eq!(eval(&p, [6; 32], 0).unwrap_err().code, "wrong_network");
    assert_eq!(eval(&p, NETWORK, 1).unwrap_err().code, "invalid");
}

#[test]
fn verifier_errors_are_reported_and_recorded() {
    let p = prover();
    ready(
        &p,
        Arc::new(FakeVerifier {
            fail: true,
            ..FakeVerifier::default()
        }),
    );
    assert_eq!(eval(&p, NETWORK, 0).unwrap_err().code, "evaluation_failed");
    let last = p.capability().last_evaluation.unwrap();
    assert!(!last.ok && !last.proved);
    // Fail closed: the verifier is withdrawn until the workers are restarted.
    assert_eq!(eval(&p, NETWORK, 0).unwrap_err().code, "not_ready");
    assert!(!p.capability().workers.ready);
}

fn identity() -> (Vec<u8>, Vec<u8>) {
    let generated = rcgen::generate_simple_self_signed(vec![wire::SERVER_NAME.to_owned()]).unwrap();
    (
        generated.cert.der().to_vec(),
        generated.signing_key.serialize_der(),
    )
}

#[test]
fn full_tls_session_hello_status_evaluate() {
    let p = prover();
    let fake = Arc::new(FakeVerifier::default());
    ready(&p, fake.clone());
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
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let p = Arc::clone(&p);
        let stop = Arc::clone(&stop);
        thread::spawn(move || p.serve(listener, tls, stop))
    };
    let client =
        wire::client_config(wire::certificate_sha256(&server_cert), node_cert, node_key).unwrap();
    let mut stream = wire::connect(address, client, Duration::from_secs(5)).unwrap();

    wire::write_message(
        &mut stream,
        &Request::Hello {
            protocol: wire::PROTOCOL.to_owned(),
            network_id: wire::hex32(&NETWORK),
            node_version: "test".to_owned(),
        },
        &[],
        0,
    )
    .unwrap();
    let (ack, _): (Response, _) = wire::read_message(&mut stream, 0).unwrap();
    let Response::HelloAck { capability, .. } = ack else {
        panic!("expected hello_ack")
    };
    assert_eq!(capability.gpu.unwrap().vram_total_mib, Some(32607));
    assert!(capability.proving_data.verified && capability.workers.ready);

    wire::write_message(&mut stream, &Request::Status, &[], MAX_REQUEST_BLOB_BYTES).unwrap();
    let (status, _): (Response, _) = wire::read_message(&mut stream, 0).unwrap();
    assert!(matches!(status, Response::Status { .. }));

    wire::write_message(
        &mut stream,
        &Request::Evaluate {
            request_id: 9,
            challenge: challenge(NETWORK),
            coinbase: coinbase(),
            total_fees_burned: 0,
            transactions: 0,
            nonce: 77,
            share_target: "ff".repeat(32),
        },
        &[],
        MAX_REQUEST_BLOB_BYTES,
    )
    .unwrap();
    let (result, blobs): (Response, _) =
        wire::read_message(&mut stream, MAX_RESPONSE_BLOB_BYTES).unwrap();
    let Response::Evaluation {
        request_id,
        work_digest,
        proof,
        ..
    } = result
    else {
        panic!("expected evaluation")
    };
    assert_eq!(request_id, 9);
    assert!(!proof && blobs.is_empty());
    assert!(work_digest.starts_with(&hex::encode(77_u64.to_be_bytes())));
    assert_eq!(fake.calls.load(Ordering::Acquire), 1);

    drop(stream);
    stop.store(true, Ordering::Release);
    server.join().unwrap();
}

#[test]
fn hello_for_another_network_is_rejected() {
    let p = prover();
    let mut input = Vec::new();
    wire::write_message(
        &mut input,
        &Request::Hello {
            protocol: wire::PROTOCOL.to_owned(),
            network_id: wire::hex32(&[6; 32]),
            node_version: "test".to_owned(),
        },
        &[],
        0,
    )
    .unwrap();
    let mut duplex = Duplex {
        input: input.as_slice(),
        output: Vec::new(),
    };
    assert_eq!(p.handle(&mut duplex).unwrap_err().code, "protocol");
    let (response, _): (Response, _) =
        wire::read_message(&mut duplex.output.as_slice(), 0).unwrap();
    assert!(matches!(response, Response::Error { .. }));
}

#[test]
fn evaluate_before_hello_is_rejected() {
    let p = prover();
    ready(&p, Arc::new(FakeVerifier::default()));
    let mut input = Vec::new();
    wire::write_message(&mut input, &Request::Status, &[], 0).unwrap();
    let mut duplex = Duplex {
        input: input.as_slice(),
        output: Vec::new(),
    };
    assert!(p.handle(&mut duplex).is_err());
}

struct Duplex<'a> {
    input: &'a [u8],
    output: Vec<u8>,
}

impl Read for Duplex<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.input.read(buf)
    }
}

impl std::io::Write for Duplex<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.output.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kraskus-prover-{label}-{}-{}",
        std::process::id(),
        unix_now()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("production-v4/fixed")).unwrap();
    dir
}

fn write_catalog(root: &Path) -> PathBuf {
    let mut rows = Vec::new();
    for (i, name) in CATALOG_NAMES.iter().enumerate() {
        let bytes = vec![i as u8; 1000 + i];
        fs::write(artifact_path(root, name), &bytes).unwrap();
        rows.push(serde_json::json!({"name": name, "bytes": bytes.len(), "sha256": hex::encode(Sha256::digest(&bytes))}));
    }
    let path = root.join("production-v4-rcnet-1-inputs.json");
    fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"schema_version": 1, "files": rows})).unwrap(),
    )
    .unwrap();
    path
}

#[test]
fn proving_data_must_match_every_size_and_sha256() {
    let root = temp_dir("data");
    let catalog = write_catalog(&root);
    let cache = root.join("cache.json");
    let mut updates = 0;
    let status = verify_proving_data(&root, &catalog, Some(&cache), |_| updates += 1);
    assert!(status.verified, "{:?}", status.detail);
    assert_eq!(status.files.len(), 8);
    assert!(updates >= 9);
    assert_eq!(status.bytes, status.expected_bytes);

    // Same size, different content: rehashed (mtime changed) and refused.
    let tree = artifact_path(&root, "FORGEMATRIX-V4-FIXED-BANK-1.tree");
    let mut bytes = fs::read(&tree).unwrap();
    bytes[0] ^= 1;
    thread::sleep(Duration::from_millis(20));
    fs::write(&tree, bytes).unwrap();
    let status = verify_proving_data(&root, &catalog, Some(&cache), |_| {});
    assert!(!status.verified);
    assert!(
        status
            .detail
            .unwrap()
            .contains("FORGEMATRIX-V4-FIXED-BANK-1.tree")
    );

    fs::remove_file(artifact_path(&root, "MODEL-V2.bank")).unwrap();
    assert!(!verify_proving_data(&root, &catalog, None, |_| {}).verified);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn unexpected_catalog_is_refused() {
    let root = temp_dir("catalog");
    let path = root.join("catalog.json");
    fs::write(
        &path,
        br#"{"schema_version":1,"files":[{"name":"x","bytes":1,"sha256":"00"}]}"#,
    )
    .unwrap();
    let status = verify_proving_data(&root, &path, None, |_| {});
    assert!(!status.verified && status.detail.unwrap().contains("8-file"));
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn worker_pins_are_exact() {
    let root = temp_dir("pins");
    let replay = root.join("cmfd-v4-replay");
    let proof = root.join("real_bank0_relations");
    fs::write(&replay, b"replay").unwrap();
    fs::write(&proof, b"proof").unwrap();
    let r = hex::encode(Sha256::digest(b"replay"));
    let q = hex::encode(Sha256::digest(b"proof"));
    assert!(check_worker_pins(&replay, &proof, &r, &q).pins_ok);
    assert!(!check_worker_pins(&replay, &proof, &q, &r).pins_ok);
    assert!(!check_worker_pins(&root.join("missing"), &proof, &r, &q).pins_ok);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn nvidia_smi_output_is_parsed() {
    let out = "NVIDIA GeForce RTX 5070, GPU-b61e, 12.0, 12227, 11800, 615.71.09\nNVIDIA GeForce RTX 5090, GPU-aaaa, 12.0, 32607, 32000, 615.71.09\n";
    assert_eq!(
        parse_nvidia_smi(out, None).unwrap().vram_total_mib,
        Some(12227)
    );
    let g = parse_nvidia_smi(out, Some("GPU-aaaa")).unwrap();
    assert_eq!(g.name, "NVIDIA GeForce RTX 5090");
    assert!(parse_nvidia_smi(out, Some("GPU-none")).is_err());
    assert!(parse_nvidia_smi("", None).is_err());
}
