//! Kraskus remote LAN prover service (Kraskus-Common-Foundry-Solo; NOT upstream).
//!
//! Runs on a GPU machine. It wraps the **unmodified** upstream
//! `ProductionV4PersistentPoolVerifier` and the official v1.0.8 workers and answers
//! exactly one kind of work: `evaluate(template, nonce, share_target)`, the same
//! call the upstream pool server makes locally. The node verifies everything that
//! comes back with its own consensus code; this service holds no wallet, no chain
//! and no node credentials.
//!
//! The service reports, and gates every evaluation on:
//! - proving data: every catalog file matches its exact size and SHA-256;
//! - workers: both binaries match their pins and printed their READY markers;
//! - GPU: name, UUID, compute capability, total/free VRAM, driver (read-only query).

use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use cmfd_node::BlockTemplate;
use cmfd_node::pool::{ProductionV4PoolShareEvaluation, ProductionV4PoolShareVerifier};
use kraskus_cmfd_prover_wire::rustls::ServerConfig;
use kraskus_cmfd_prover_wire::{
    self as wire, Capability, DataStatus, EvaluationRecord, FileCheck, GpuInfo, Request, Response,
    WireError, WorkerStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+kraskus-solo");
pub const MAX_CONNECTIONS: usize = 4;
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(300);

/// The eight official proving inputs, exactly as the upstream mainnet pool
/// service names and locates them (`mainnet-pool-service.py`).
pub const CATALOG_NAMES: [&str; 8] = [
    "MODEL-V2.bank",
    "FORGEMATRIX-V4-FIXED-ARTIFACT-RECORD-V1.json",
    "FORGEMATRIX-V4-FIXED-BANK-0.row-major.codeword",
    "FORGEMATRIX-V4-FIXED-BANK-0.tree",
    "FORGEMATRIX-V4-FIXED-BANK-1.row-major.codeword",
    "FORGEMATRIX-V4-FIXED-BANK-1.tree",
    "FORGEMATRIX-V4-FIXED-BANK-2.row-major.codeword",
    "FORGEMATRIX-V4-FIXED-BANK-2.tree",
];

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ------------------------------------------------------------ proving data

#[derive(Debug, Clone, Deserialize)]
struct CatalogRow {
    name: String,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
struct Catalog {
    schema_version: u32,
    files: Vec<CatalogRow>,
}

/// Same mapping as upstream: the bank in `production-v4/`, everything else in
/// `production-v4/fixed/`.
pub fn artifact_path(runtime: &Path, name: &str) -> PathBuf {
    let dir = runtime.join("production-v4");
    if name == "MODEL-V2.bank" {
        dir.join(name)
    } else {
        dir.join("fixed").join(name)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct CacheEntry {
    name: String,
    bytes: u64,
    modified_ns: u128,
    sha256: String,
}

fn modified_ns(meta: &fs::Metadata) -> u128 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

pub fn sha256_file(path: &Path) -> std::io::Result<(String, u64)> {
    let mut reader = BufReader::with_capacity(8 * 1024 * 1024, File::open(path)?);
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 8 * 1024 * 1024];
    let mut total = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        total += read as u64;
    }
    Ok((hex::encode(hasher.finalize()), total))
}

/// Verifies every catalog file by exact size and SHA-256. A cache entry is used
/// only when the file's size and modification time are unchanged.
pub fn verify_proving_data(
    runtime: &Path,
    catalog_path: &Path,
    cache_path: Option<&Path>,
    mut progress: impl FnMut(&DataStatus),
) -> DataStatus {
    let mut status = DataStatus::default();
    let catalog: Catalog = match fs::read(catalog_path)
        .map_err(|e| e.to_string())
        .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
    {
        Ok(catalog) => catalog,
        Err(error) => {
            status.detail = Some(format!("input catalog unreadable: {error}"));
            return status;
        }
    };
    let mut names: Vec<_> = catalog.files.iter().map(|r| r.name.as_str()).collect();
    names.sort_unstable();
    let mut expected: Vec<_> = CATALOG_NAMES.to_vec();
    expected.sort_unstable();
    if catalog.schema_version != 1 || names != expected {
        status.detail =
            Some("input catalog is not the expected 8-file ProductionV4 catalog".to_owned());
        return status;
    }
    let cache: Vec<CacheEntry> = cache_path
        .and_then(|p| fs::read(p).ok())
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let mut fresh_cache = Vec::new();
    status.expected_bytes = catalog.files.iter().map(|r| r.bytes).sum();
    let mut all_ok = true;
    for (index, row) in catalog.files.iter().enumerate() {
        let path = artifact_path(runtime, &row.name);
        status.detail = Some(format!("verifying {} of 8: {}", index + 1, row.name));
        progress(&status);
        let meta = fs::metadata(&path);
        let (ok, bytes) = match meta {
            Err(_) => (false, 0),
            Ok(meta) if meta.len() != row.bytes => (false, meta.len()),
            Ok(meta) => {
                let stamp = modified_ns(&meta);
                let cached = cache.iter().find(|c| {
                    c.name == row.name
                        && c.bytes == row.bytes
                        && c.modified_ns == stamp
                        && c.sha256 == row.sha256
                });
                let digest = match cached {
                    Some(c) => Some(c.sha256.clone()),
                    None => sha256_file(&path).ok().map(|(d, _)| d),
                };
                let ok = digest.as_deref() == Some(row.sha256.as_str());
                if ok {
                    fresh_cache.push(CacheEntry {
                        name: row.name.clone(),
                        bytes: row.bytes,
                        modified_ns: stamp,
                        sha256: row.sha256.clone(),
                    });
                }
                (ok, meta.len())
            }
        };
        all_ok &= ok;
        status.bytes += if ok { bytes } else { 0 };
        status.files.push(FileCheck {
            name: row.name.clone(),
            bytes,
            expected_bytes: row.bytes,
            ok,
        });
    }
    if let Some(p) = cache_path
        && let Ok(bytes) = serde_json::to_vec_pretty(&fresh_cache)
    {
        let _ = fs::write(p, bytes);
    }
    status.verified = all_ok;
    status.checked_unix = Some(unix_now());
    status.detail = if all_ok {
        None
    } else {
        let bad: Vec<_> = status
            .files
            .iter()
            .filter(|f| !f.ok)
            .map(|f| f.name.clone())
            .collect();
        Some(format!("missing or mismatched: {}", bad.join(", ")))
    };
    progress(&status);
    status
}

// ---------------------------------------------------------------- workers

pub fn check_worker_pins(
    replay: &Path,
    proof: &Path,
    expected_replay: &str,
    expected_proof: &str,
) -> WorkerStatus {
    let replay_sha = sha256_file(replay).ok().map(|(d, _)| d);
    let proof_sha = sha256_file(proof).ok().map(|(d, _)| d);
    let pins_ok = replay_sha.as_deref() == Some(&expected_replay.to_ascii_lowercase())
        && proof_sha.as_deref() == Some(&expected_proof.to_ascii_lowercase());
    WorkerStatus {
        ready: false,
        pins_ok,
        detail: (!pins_ok).then(|| "worker binaries do not match their pinned SHA-256".to_owned()),
        replay_sha256: replay_sha,
        proof_sha256: proof_sha,
    }
}

// -------------------------------------------------------------------- GPU

pub trait GpuProbe: Send + Sync {
    fn probe(&self) -> Result<GpuInfo, String>;
}

/// Read-only `nvidia-smi --query-gpu` (no settings are changed).
pub struct NvidiaSmi {
    pub program: PathBuf,
    pub uuid: Option<String>,
}

pub fn parse_nvidia_smi(output: &str, uuid: Option<&str>) -> Result<GpuInfo, String> {
    let gpus: Vec<GpuInfo> = output
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|line| {
            let f: Vec<_> = line.split(',').map(str::trim).collect();
            (f.len() >= 6).then(|| GpuInfo {
                name: f[0].to_owned(),
                uuid: Some(f[1].to_owned()),
                compute_capability: Some(f[2].to_owned()),
                vram_total_mib: f[3].parse().ok(),
                vram_free_mib: f[4].parse().ok(),
                driver_version: Some(f[5].to_owned()),
            })
        })
        .collect();
    match uuid {
        Some(want) => gpus
            .into_iter()
            .find(|g| g.uuid.as_deref() == Some(want))
            .ok_or_else(|| format!("GPU {want} not found")),
        None => gpus
            .into_iter()
            .next()
            .ok_or_else(|| "no NVIDIA GPU found".to_owned()),
    }
}

impl GpuProbe for NvidiaSmi {
    fn probe(&self) -> Result<GpuInfo, String> {
        let output = Command::new(&self.program)
            .args([
                "--query-gpu=name,uuid,compute_cap,memory.total,memory.free,driver_version",
                "--format=csv,noheader,nounits",
            ])
            .output()
            .map_err(|e| format!("nvidia-smi unavailable: {e}"))?;
        if !output.status.success() {
            return Err("nvidia-smi failed".to_owned());
        }
        parse_nvidia_smi(
            &String::from_utf8_lossy(&output.stdout),
            self.uuid.as_deref(),
        )
    }
}

// ----------------------------------------------------------------- service

pub struct ProverIdentity {
    pub network_id: [u8; 32],
    pub upstream_commit: String,
}

#[derive(Default)]
struct State {
    data: DataStatus,
    workers: WorkerStatus,
    last_evaluation: Option<EvaluationRecord>,
}

pub struct Prover {
    identity: ProverIdentity,
    gpu: Box<dyn GpuProbe>,
    state: Mutex<State>,
    verifier: RwLock<Option<Arc<dyn ProductionV4PoolShareVerifier>>>,
    evaluate_lock: Mutex<()>,
    busy: AtomicBool,
    connections: AtomicUsize,
}

impl Prover {
    pub fn new(identity: ProverIdentity, gpu: Box<dyn GpuProbe>) -> Arc<Self> {
        Arc::new(Self {
            identity,
            gpu,
            state: Mutex::new(State::default()),
            verifier: RwLock::new(None),
            evaluate_lock: Mutex::new(()),
            busy: AtomicBool::new(false),
            connections: AtomicUsize::new(0),
        })
    }

    pub fn set_data(&self, data: DataStatus) {
        if let Ok(mut s) = self.state.lock() {
            s.data = data;
        }
    }

    pub fn set_workers(&self, workers: WorkerStatus) {
        if let Ok(mut s) = self.state.lock() {
            s.workers = workers;
        }
    }

    pub fn data(&self) -> DataStatus {
        self.state
            .lock()
            .map(|s| s.data.clone())
            .unwrap_or_default()
    }

    pub fn workers(&self) -> WorkerStatus {
        self.state
            .lock()
            .map(|s| s.workers.clone())
            .unwrap_or_default()
    }

    /// Installs the started upstream verifier; the workers are READY at this point.
    pub fn install_verifier(&self, verifier: Arc<dyn ProductionV4PoolShareVerifier>) {
        if let Ok(mut slot) = self.verifier.write() {
            *slot = Some(verifier);
        }
        if let Ok(mut s) = self.state.lock() {
            s.workers.ready = true;
            s.workers.detail = None;
        }
    }

    /// Removes the verifier after a worker failure; evaluations fail closed.
    pub fn withdraw_verifier(&self, reason: String) {
        if let Ok(mut slot) = self.verifier.write() {
            *slot = None;
        }
        if let Ok(mut s) = self.state.lock() {
            s.workers.ready = false;
            s.workers.detail = Some(reason);
        }
    }

    pub fn capability(&self) -> Capability {
        let (data, workers, last) = self
            .state
            .lock()
            .map(|s| (s.data.clone(), s.workers.clone(), s.last_evaluation.clone()))
            .unwrap_or_default();
        Capability {
            prover_version: VERSION.to_owned(),
            upstream_commit: self.identity.upstream_commit.clone(),
            network_id: wire::hex32(&self.identity.network_id),
            gpu: self.gpu.probe().ok(),
            proving_data: data,
            workers,
            busy: self.busy.load(Ordering::Acquire),
            last_evaluation: last,
            reported_unix: unix_now(),
        }
    }

    fn ready_verifier(&self) -> Result<Arc<dyn ProductionV4PoolShareVerifier>, WireError> {
        let (data_ok, workers) = self
            .state
            .lock()
            .map(|s| (s.data.verified, s.workers.clone()))
            .map_err(|_| WireError::new("internal", "state poisoned"))?;
        if !data_ok {
            return Err(WireError::new("not_ready", "proving data is not verified"));
        }
        if !workers.pins_ok || !workers.ready {
            return Err(WireError::new(
                "not_ready",
                "official workers are not ready",
            ));
        }
        self.verifier
            .read()
            .ok()
            .and_then(|v| v.clone())
            .ok_or_else(|| WireError::new("not_ready", "official workers are not ready"))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn evaluate(
        &self,
        challenge: cmfd_consensus::BlockChallenge,
        coinbase: cmfd_consensus::Coinbase,
        total_fees_burned: u64,
        declared_transactions: usize,
        blobs: &[Vec<u8>],
        nonce: u64,
        share_target: &str,
    ) -> Result<(ProductionV4PoolShareEvaluation, f64), WireError> {
        let verifier = self.ready_verifier()?;
        if challenge.network_id != self.identity.network_id {
            return Err(WireError::new(
                "wrong_network",
                "template is for another network",
            ));
        }
        if blobs.len() != declared_transactions {
            return Err(WireError::new(
                "invalid",
                "transaction count does not match",
            ));
        }
        let transactions = wire::decode_transactions(blobs, self.identity.network_id)?;
        let share_target = wire::parse_hex32(share_target)?;
        let template = BlockTemplate {
            challenge,
            coinbase,
            transactions,
            total_fees_burned,
        };
        let _guard = self
            .evaluate_lock
            .lock()
            .map_err(|_| WireError::new("internal", "evaluation lock poisoned"))?;
        self.busy.store(true, Ordering::Release);
        let started = Instant::now();
        let result = verifier.evaluate(&template, nonce, share_target);
        let seconds = started.elapsed().as_secs_f64();
        self.busy.store(false, Ordering::Release);
        let record = EvaluationRecord {
            at_unix: unix_now(),
            ok: result.is_ok(),
            seconds,
            proved: result
                .as_ref()
                .map(|r| r.chain_proof.is_some())
                .unwrap_or(false),
        };
        if let Ok(mut s) = self.state.lock() {
            s.last_evaluation = Some(record);
        }
        if let Err(error) = &result {
            // Fail closed: no further evaluations until the workers are re-checked and restarted.
            self.withdraw_verifier(format!("last evaluation failed: {error}"));
        }
        result
            .map(|r| (r, seconds))
            .map_err(|e| WireError::new("evaluation_failed", e.to_string()))
    }

    /// One authenticated node connection: `hello` first, then status/evaluate.
    pub fn handle<S: Read + std::io::Write>(&self, stream: &mut S) -> Result<(), WireError> {
        let network = wire::hex32(&self.identity.network_id);
        let (hello, _): (Request, _) = wire::read_message(stream, 0)?;
        match hello {
            Request::Hello {
                protocol,
                network_id,
                ..
            } if protocol == wire::PROTOCOL && network_id == network => {
                wire::write_message(
                    stream,
                    &Response::HelloAck {
                        protocol: wire::PROTOCOL.to_owned(),
                        network_id: network.clone(),
                        capability: self.capability(),
                    },
                    &[],
                    0,
                )?;
            }
            _ => {
                let _ = send_error(
                    stream,
                    "protocol",
                    "expected hello for this protocol and network",
                );
                return Err(WireError::new("protocol", "bad hello"));
            }
        }
        loop {
            let (request, blobs): (Request, _) =
                match wire::read_message(stream, wire::MAX_REQUEST_BLOB_BYTES) {
                    Ok(message) => message,
                    Err(e) if e.code == "closed" => return Ok(()),
                    Err(e) => return Err(e),
                };
            match request {
                Request::Status => wire::write_message(
                    stream,
                    &Response::Status {
                        capability: self.capability(),
                    },
                    &[],
                    0,
                )?,
                Request::Evaluate {
                    request_id,
                    challenge,
                    coinbase,
                    total_fees_burned,
                    transactions,
                    nonce,
                    share_target,
                } => match self.evaluate(
                    challenge,
                    coinbase,
                    total_fees_burned,
                    transactions,
                    &blobs,
                    nonce,
                    &share_target,
                ) {
                    Ok((evaluation, seconds)) => {
                        let proof_blobs = match &evaluation.chain_proof {
                            Some(proof) => {
                                vec![wire::encode_proof(proof, self.identity.network_id)?]
                            }
                            None => vec![],
                        };
                        wire::write_message(
                            stream,
                            &Response::Evaluation {
                                request_id,
                                work_digest: wire::hex32(&evaluation.work_digest),
                                proof: evaluation.chain_proof.is_some(),
                                seconds,
                            },
                            &proof_blobs,
                            wire::MAX_RESPONSE_BLOB_BYTES,
                        )?
                    }
                    Err(e) => send_error(stream, e.code, &e.message)?,
                },
                Request::Hello { .. } => send_error(stream, "protocol", "hello already completed")?,
            }
        }
    }

    /// Accept loop. Peers must be on private addresses and present an
    /// allow-listed certificate; at most `MAX_CONNECTIONS` at once.
    pub fn serve(
        self: &Arc<Self>,
        listener: TcpListener,
        tls: Arc<ServerConfig>,
        stop: Arc<AtomicBool>,
    ) {
        listener.set_nonblocking(true).ok();
        while !stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((socket, peer)) => {
                    if !wire::is_private(peer.ip())
                        || self.connections.load(Ordering::Acquire) >= MAX_CONNECTIONS
                    {
                        drop(socket);
                        continue;
                    }
                    let _ = socket.set_nonblocking(false);
                    let prover = Arc::clone(self);
                    let tls = Arc::clone(&tls);
                    prover.connections.fetch_add(1, Ordering::AcqRel);
                    thread::spawn(move || {
                        match wire::accept(socket, tls, IDLE_TIMEOUT) {
                            Ok(mut stream) => {
                                if let Err(e) = prover.handle(&mut stream) {
                                    eprintln!("kraskus prover: connection from {peer} ended: {e}");
                                }
                            }
                            Err(e) => eprintln!("kraskus prover: refused {peer}: {e}"),
                        }
                        prover.connections.fetch_sub(1, Ordering::AcqRel);
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(50))
                }
                Err(e) => {
                    eprintln!("kraskus prover: accept failed: {e}");
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }
    }
}

fn send_error<S: std::io::Write>(
    stream: &mut S,
    code: &str,
    message: &str,
) -> Result<(), WireError> {
    wire::write_message(
        stream,
        &Response::Error {
            code: code.to_owned(),
            message: message.to_owned(),
        },
        &[],
        0,
    )
}

/// Starts the unmodified upstream verifier with the official workers of the
/// signed runtime package (same commands as upstream `pool-serve`).
#[cfg(feature = "production-mainnet")]
pub fn start_official_verifier(
    runtime: &Path,
    scratch: &Path,
    gpu_uuid: Option<&str>,
) -> Result<Arc<dyn ProductionV4PoolShareVerifier>, String> {
    use cmfd_node::production_v4_pool::{
        ProductionV4PersistentPoolVerifier, ProductionV4PoolVerifierConfig,
        ProductionV4PoolWorkerCommand,
    };
    let dir = runtime.join("production-v4");
    let bank = dir.join("MODEL-V2.bank");
    let mut environment = Vec::new();
    if let Some(uuid) = gpu_uuid {
        environment.push(("CUDA_VISIBLE_DEVICES".into(), uuid.into()));
    }
    let lib = runtime.join("lib");
    if lib.is_dir() {
        environment.push(("LD_LIBRARY_PATH".into(), lib.into_os_string()));
    }
    if !scratch.is_absolute() {
        return Err("scratch directory must be absolute".to_owned());
    }
    let config = ProductionV4PoolVerifierConfig {
        replay: ProductionV4PoolWorkerCommand {
            program: dir.join("cmfd-v4-replay"),
            arguments: vec!["--server".into(), bank.clone().into_os_string()],
            environment: environment.clone(),
        },
        proof: ProductionV4PoolWorkerCommand {
            program: dir.join("real_bank0_relations"),
            arguments: vec![
                "--server".into(),
                hex::encode(cmfd_node::COMPILED_NETWORK_PROFILE.network_id).into(),
                bank.into_os_string(),
                dir.clone().into_os_string(),
            ],
            environment,
        },
        scratch_directory: scratch.to_path_buf(),
        worker_scratch_directory: scratch
            .to_str()
            .ok_or_else(|| "scratch path is not UTF-8".to_owned())?
            .to_owned(),
    };
    ProductionV4PersistentPoolVerifier::start(config)
        .map(|v| Arc::new(v) as Arc<dyn ProductionV4PoolShareVerifier>)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
