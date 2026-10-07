//! Kraskus solo endpoint for `cmfd-node run` (Kraskus-Common-Foundry-Solo).
//!
//! NOT UPSTREAM. Together with `kraskus_remote.rs` this is the whole Kraskus
//! change apart from the small hook in `main.rs`. It serves the unmodified
//! upstream pool protocol from the normal `run` mode (P2P, loopback RPC, wallet
//! and explorer stay up) through the unmodified `spawn_pool_server`, configured
//! for one solo operator:
//!
//! - the block reward pays the operator's own address (the node wallet by
//!   default), with no payout ledger, no PPLNS and no payout transactions;
//! - the share target is the network target, so only block-winning nonces are
//!   submitted, then replayed and proven by the official ProductionV4 workers on
//!   a Kraskus remote prover (LAN GPU host, or loopback on the same machine);
//! - the miner endpoint listens **only while Solo Prover Ready holds**: prover
//!   connected and mutually authenticated, GPU memory sufficient, proving data
//!   verified, workers ready, and a proof self-test verified by this node within
//!   the last 24 h and within 30 s. Any failure closes the endpoint.
//!
//! Consensus, the PoW algorithm, block assembly and the wire protocol are the
//! upstream code paths, unchanged.

use std::error::Error;
use std::fs;
use std::io::Write;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use clap::Args;
use cmfd_node::pool::{PoolServerConfig, PoolServerHandle, certificate_sha256, spawn_pool_server};
use cmfd_node::pool_dashboard::{PoolDashboardConfig, PoolDashboardHandle, spawn_pool_dashboard};
use cmfd_node::{Node, ProductionV4VerifierArtifacts, parse_miner_destination, unix_time_seconds};
use serde_json::{Value, json};

use crate::kraskus_remote::{
    self as remote, EVALUATE_TIMEOUT, ProofCheck, RemoteProver, RemoteVerifier, SELF_TEST_INTERVAL,
    SelfTest,
};

/// `--version` names the Kraskus build so it is never mistaken for the
/// official upstream binary of the same release.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+kraskus-solo.1");

pub const SOLO_STATUS_SCHEMA: &str = "KRASKUS_CMFD_SOLO_STATUS_V1";

/// The hardest possible target. `spawn_pool_server` uses the easier of the
/// configured and chain targets, so this yields exactly the network target.
pub const SOLO_SHARE_TARGET: [u8; 32] = [0; 32];

const HEARTBEAT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Default, Args)]
pub struct SoloPoolArgs {
    /// Kraskus solo: serve the pool protocol for solo mining from `run` on this
    /// private (LAN or loopback) address. Off unless supplied.
    #[arg(long, requires_all = ["solo_pool_certificate", "solo_pool_private_key", "solo_pool_prover_client_certificate", "solo_pool_prover_client_key"])]
    pub solo_pool_bind: Option<SocketAddr>,
    /// DER certificate of the miner endpoint (pool-certificate).
    #[arg(long, requires = "solo_pool_bind")]
    pub solo_pool_certificate: Option<PathBuf>,
    /// DER PKCS#8 private key of the miner endpoint.
    #[arg(long, requires = "solo_pool_bind")]
    pub solo_pool_private_key: Option<PathBuf>,
    /// 64-hex block-reward destination; defaults to the node wallet.
    #[arg(long, requires = "solo_pool_bind")]
    pub solo_pool_miner: Option<String>,
    /// Kraskus prover URL: cmfd-prover+tls://<private IP>:<port>?pin=<sha256>.
    #[arg(
        long,
        requires = "solo_pool_bind",
        conflicts_with = "solo_pool_remote_prover_file"
    )]
    pub solo_pool_remote_prover: Option<String>,
    /// File holding the prover URL (plain text or {"url": ...}); re-read every
    /// heartbeat so pairing changes need no node restart. Missing = no prover.
    #[arg(long, requires = "solo_pool_bind")]
    pub solo_pool_remote_prover_file: Option<PathBuf>,
    /// DER certificate this node presents to the prover (pool-certificate).
    #[arg(long, requires = "solo_pool_bind")]
    pub solo_pool_prover_client_certificate: Option<PathBuf>,
    /// DER PKCS#8 key for the prover client certificate.
    #[arg(long, requires = "solo_pool_bind")]
    pub solo_pool_prover_client_key: Option<PathBuf>,
    /// Status JSON rewritten every heartbeat (schema KRASKUS_CMFD_SOLO_STATUS_V1).
    #[arg(long, requires = "solo_pool_bind")]
    pub solo_pool_status_file: Option<PathBuf>,
    /// Upstream pool dashboard snapshot (workers, work rate, job) rewritten every heartbeat.
    #[arg(long, requires = "solo_pool_bind")]
    pub solo_pool_snapshot_file: Option<PathBuf>,
    /// Minimum seconds between proof self-test attempts after a failure.
    #[arg(long, default_value_t = 60, requires = "solo_pool_bind")]
    pub solo_pool_retry_seconds: u64,
    /// Loopback address for the upstream read-only pool dashboard (`/api/v1/pool`).
    #[arg(long, requires_all = ["solo_pool_bind", "solo_pool_dashboard_assets", "solo_pool_public_url"])]
    pub solo_pool_dashboard_bind: Option<SocketAddr>,
    /// Upstream dashboard asset directory.
    #[arg(long, requires = "solo_pool_dashboard_bind")]
    pub solo_pool_dashboard_assets: Option<PathBuf>,
    /// cmfd+tls URL shown by the dashboard; its pin must match the certificate.
    #[arg(long, requires = "solo_pool_dashboard_bind")]
    pub solo_pool_public_url: Option<String>,
}

#[derive(Default)]
struct Services {
    pool: Option<PoolServerHandle>,
    dashboard: Option<PoolDashboardHandle>,
}

impl Services {
    fn stop(&mut self) -> Result<(), Box<dyn Error>> {
        let dashboard = self.dashboard.take().map(PoolDashboardHandle::stop);
        let pool = self.pool.take().map(PoolServerHandle::stop);
        if let Some(result) = dashboard {
            result?;
        }
        if let Some(result) = pool {
            result?;
        }
        Ok(())
    }
}

/// Background owner of the solo endpoint. The node keeps running whatever the
/// endpoint state is; only a crash of a started endpoint ends the process,
/// exactly as `pool-serve` treats its own pool thread.
pub struct SoloPool {
    stop: Arc<AtomicBool>,
    services: Arc<Mutex<Services>>,
    supervisor: Option<JoinHandle<()>>,
    status: StatusWriter,
}

impl SoloPool {
    pub fn startup_json(&self) -> Value {
        self.status.current()
    }

    pub fn is_finished(&self) -> bool {
        self.services.lock().is_ok_and(|services| {
            services
                .pool
                .as_ref()
                .is_some_and(PoolServerHandle::is_finished)
                || services
                    .dashboard
                    .as_ref()
                    .is_some_and(PoolDashboardHandle::is_finished)
        })
    }

    pub fn stop(mut self) -> Result<(), Box<dyn Error>> {
        self.stop.store(true, Ordering::Release);
        if let Some(supervisor) = self.supervisor.take() {
            let _ = supervisor.join();
        }
        let result = self
            .services
            .lock()
            .map_err(|_| "solo pool state poisoned")?
            .stop();
        self.status.update("stopped", None, None, None);
        result
    }
}

struct Endpoint {
    bind: SocketAddr,
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
    pin: [u8; 32],
    destination: [u8; 32],
}

/// Validates the static options and starts the endpoint supervisor. Returns
/// `Ok(None)` when `--solo-pool-bind` is absent (upstream behaviour).
pub fn spawn(
    args: SoloPoolArgs,
    node: Arc<Mutex<Node>>,
    artifacts: Option<ProductionV4VerifierArtifacts>,
) -> Result<Option<SoloPool>, Box<dyn Error>> {
    let Some(bind) = args.solo_pool_bind else {
        return Ok(None);
    };
    let certificate_der = fs::read(args.solo_pool_certificate.as_ref().ok_or("certificate")?)?;
    let private_key_der = fs::read(args.solo_pool_private_key.as_ref().ok_or("private key")?)?;
    let pin = certificate_sha256(&certificate_der);
    let (destination, network_id) = {
        let node = node.lock().map_err(|_| "shared node poisoned")?;
        let destination = match args.solo_pool_miner.as_deref() {
            Some(value) => parse_miner_destination(value)?,
            None => node.wallet_destination(),
        };
        (destination, node.network_profile().network_id)
    };
    if args.solo_pool_retry_seconds == 0 {
        return Err("solo-pool-retry-seconds must be at least 1".into());
    }
    let client_certificate_der = fs::read(
        args.solo_pool_prover_client_certificate
            .as_ref()
            .ok_or("client certificate")?,
    )?;
    let client_key_der = fs::read(
        args.solo_pool_prover_client_key
            .as_ref()
            .ok_or("client key")?,
    )?;
    let client_pin = certificate_sha256(&client_certificate_der);
    let source = match (
        &args.solo_pool_remote_prover,
        &args.solo_pool_remote_prover_file,
    ) {
        (Some(url), None) => {
            kraskus_cmfd_prover_wire::parse_url(url)?;
            ProverSource::Fixed(url.clone())
        }
        (None, Some(path)) => ProverSource::File(path.clone()),
        _ => {
            return Err(
                "exactly one of --solo-pool-remote-prover or --solo-pool-remote-prover-file is required".into(),
            );
        }
    };
    let status = StatusWriter::new(
        args.solo_pool_status_file.clone(),
        json!({
            "schema": SOLO_STATUS_SCHEMA,
            "endpoint": bind.to_string(),
            "certificate_sha256": hex::encode(pin),
            "block_reward_destination": hex::encode(destination),
            "share_target": "network",
            "payouts": "none (solo: the block reward pays the destination directly)",
            "prover_url": Value::Null,
            "client_certificate_sha256": hex::encode(client_pin),
        }),
    );
    status.update("starting", None, None, None);
    let stop = Arc::new(AtomicBool::new(false));
    let services = Arc::new(Mutex::new(Services::default()));
    let endpoint = Endpoint {
        bind,
        certificate_der,
        private_key_der,
        pin,
        destination,
    };
    let supervisor = {
        let stop = Arc::clone(&stop);
        let services = Arc::clone(&services);
        let status = status.clone();
        thread::Builder::new()
            .name("kraskus-solo-pool".to_owned())
            .spawn(move || {
                supervise(Supervisor {
                    args,
                    endpoint,
                    network_id,
                    node,
                    source,
                    client_certificate_der,
                    client_key_der,
                    prover: None,
                    artifacts,
                    check: None,
                    services,
                    status,
                    stop,
                })
            })?
    };
    Ok(Some(SoloPool {
        stop,
        services,
        supervisor: Some(supervisor),
        status,
    }))
}

struct Supervisor {
    args: SoloPoolArgs,
    endpoint: Endpoint,
    network_id: [u8; 32],
    node: Arc<Mutex<Node>>,
    source: ProverSource,
    client_certificate_der: Vec<u8>,
    client_key_der: Vec<u8>,
    prover: Option<Arc<RemoteProver>>,
    artifacts: Option<ProductionV4VerifierArtifacts>,
    check: Option<Box<ProofCheck>>,
    services: Arc<Mutex<Services>>,
    status: StatusWriter,
    stop: Arc<AtomicBool>,
}

enum ProverSource {
    Fixed(String),
    File(PathBuf),
}

const NO_PROVER: &str = "no block prover configured";

impl ProverSource {
    /// The configured prover URL, or why there is none.
    fn url(&self) -> Result<String, String> {
        match self {
            Self::Fixed(url) => Ok(url.clone()),
            Self::File(path) => {
                let text = fs::read_to_string(path).map_err(|_| NO_PROVER.to_owned())?;
                let text = text.trim();
                if text.is_empty() {
                    return Err(NO_PROVER.to_owned());
                }
                if text.starts_with('{') {
                    serde_json::from_str::<Value>(text)
                        .ok()
                        .and_then(|v| v.get("url").and_then(Value::as_str).map(str::to_owned))
                        .filter(|u| !u.is_empty())
                        .ok_or_else(|| NO_PROVER.to_owned())
                } else {
                    Ok(text.to_owned())
                }
            }
        }
    }
}

fn proof_check(
    artifacts: Option<&ProductionV4VerifierArtifacts>,
    network_id: [u8; 32],
) -> Result<Box<ProofCheck>, String> {
    #[cfg(feature = "production-v4")]
    {
        let artifacts = artifacts.ok_or("this node has no ProductionV4 model bank")?;
        remote::upstream_proof_check(artifacts, network_id)
    }
    #[cfg(not(feature = "production-v4"))]
    {
        let _ = (artifacts, network_id);
        Err("this node build has no ProductionV4 verifier".to_owned())
    }
}

fn supervise(mut s: Supervisor) {
    let retry = Duration::from_secs(s.args.solo_pool_retry_seconds);
    let mut capability = None;
    let mut reported_at: Option<Instant> = None;
    let mut status_error: Option<String> = None;
    let mut self_test: Option<SelfTest> = None;
    let mut last_attempt: Option<Instant> = None;
    while !s.stop.load(Ordering::Acquire) {
        // 0. Pairing. A changed prover URL replaces the client and resets every
        //    piece of prover state: nothing carries over to a different prover.
        let wanted = s.source.url();
        let current = s.prover.as_ref().map(|p| p.url());
        let changed = match (&wanted, &current) {
            (Ok(w), Some(c)) => w != c,
            (Ok(_), None) => true,
            (Err(_), Some(_)) => true,
            (Err(_), None) => status_error.is_none(),
        };
        if changed {
            capability = None;
            reported_at = None;
            self_test = None;
            last_attempt = None;
            s.prover = None;
            if let Ok(mut services) = s.services.lock()
                && services.pool.is_some()
            {
                eprintln!("kraskus solo pool: prover pairing changed; closing the miner endpoint");
                let _ = services.stop();
            }
            status_error = None;
            match &wanted {
                Ok(url) => match RemoteProver::new(
                    url,
                    s.network_id,
                    s.client_certificate_der.clone(),
                    s.client_key_der.clone(),
                ) {
                    Ok(prover) => s.prover = Some(Arc::new(prover)),
                    Err(e) => status_error = Some(format!("invalid prover URL: {e}")),
                },
                Err(reason) => status_error = Some(reason.clone()),
            }
            s.status.set(
                "prover_url",
                s.prover.as_ref().map_or(Value::Null, |p| json!(p.url())),
            );
        }
        // 1. Heartbeat. A status poll never waits behind an evaluation; an
        //    evaluation in flight within its deadline counts as liveness.
        let heartbeat = match &s.prover {
            Some(prover) => prover.status(),
            None => Err(kraskus_cmfd_prover_wire::WireError::new(
                "unconfigured",
                status_error.clone().unwrap_or_else(|| NO_PROVER.to_owned()),
            )),
        };
        match heartbeat {
            Ok(c) => {
                capability = Some(c);
                reported_at = Some(Instant::now());
                status_error = None;
            }
            Err(e)
                if e.code == "busy"
                    && s.prover
                        .as_ref()
                        .and_then(|p| p.evaluation_in_flight())
                        .is_some_and(|d| d < EVALUATE_TIMEOUT) =>
            {
                reported_at = Some(Instant::now());
            }
            Err(e) if e.code == "unconfigured" => status_error = Some(e.message),
            Err(e) => status_error = Some(e.to_string()),
        }
        let url = s.prover.as_ref().map(|p| p.url()).unwrap_or_default();
        let now = unix_time_seconds().unwrap_or(0);
        let failure = s.prover.as_ref().and_then(|p| p.failure());

        // 2. Self-test when everything else is ready and one is due.
        let assumed = SelfTest {
            at_unix: now,
            at: Instant::now(),
            ok: true,
            total_seconds: 0.0,
            error: None,
        };
        let base = remote::assess(
            &url,
            capability.as_ref(),
            reported_at,
            status_error.as_deref(),
            Some(&assumed),
            None,
            s.network_id,
            now,
        );
        let due = match &self_test {
            None => true,
            Some(t) => !t.ok || t.at.elapsed() >= SELF_TEST_INTERVAL || failure.is_some(),
        };
        let may_retry = last_attempt.is_none_or(|t| t.elapsed() >= retry);
        if base.ready && due && may_retry {
            last_attempt = Some(Instant::now());
            self_test = Some(run_self_test(&mut s, now));
            reported_at = Some(Instant::now());
        }

        // 3. Decide, fail closed.
        let failure = s.prover.as_ref().and_then(|p| p.failure());
        let assessment = remote::assess(
            &url,
            capability.as_ref(),
            reported_at,
            status_error.as_deref(),
            self_test.as_ref(),
            failure.as_deref(),
            s.network_id,
            now,
        );
        let listening = reconcile(&s, assessment.ready);

        // 4. Report.
        let (state, error) = match &listening {
            Ok(Some(_)) => ("ready", None),
            Ok(None) => ("prover_unavailable", assessment.reasons.first().cloned()),
            Err(e) => ("endpoint_error", Some(e.clone())),
        };
        s.status.update(
            state,
            error,
            listening
                .ok()
                .flatten()
                .map(|address| json!({"listening": address})),
            Some(assessment.report),
        );
        write_snapshot(&s);

        let deadline = Instant::now() + HEARTBEAT;
        while Instant::now() < deadline && !s.stop.load(Ordering::Acquire) {
            thread::sleep(Duration::from_millis(250));
        }
    }
}

fn run_self_test(s: &mut Supervisor, now: u64) -> SelfTest {
    let fail = |error: String| SelfTest {
        at_unix: now,
        at: Instant::now(),
        ok: false,
        total_seconds: 0.0,
        error: Some(error),
    };
    if s.check.is_none() {
        match proof_check(s.artifacts.as_ref(), s.network_id) {
            Ok(check) => s.check = Some(check),
            Err(e) => return fail(format!("local proof verifier unavailable: {e}")),
        }
    }
    let template = match s.node.lock() {
        Ok(node) => node.build_template(s.endpoint.destination, now),
        Err(_) => return fail("shared node poisoned".to_owned()),
    };
    let template = match template {
        Ok(t) => t,
        Err(e) => return fail(format!("test template unavailable: {e}")),
    };
    let mut nonce = [0_u8; 8];
    if getrandom::fill(&mut nonce).is_err() {
        return fail("no randomness for the test nonce".to_owned());
    }
    let Some(prover) = s.prover.clone() else {
        return fail(NO_PROVER.to_owned());
    };
    let check = s.check.as_deref().expect("proof check present");
    let result = remote::self_test(&prover, template, u64::from_le_bytes(nonce), check, now);
    eprintln!(
        "kraskus solo pool: proof self-test {} in {:.1} s{}",
        if result.ok { "passed" } else { "FAILED" },
        result.total_seconds,
        result
            .error
            .as_deref()
            .map(|e| format!(": {e}"))
            .unwrap_or_default()
    );
    result
}

/// Opens the endpoint when ready, closes it when not. Returns the listening address.
fn reconcile(s: &Supervisor, ready: bool) -> Result<Option<String>, String> {
    let mut services = s
        .services
        .lock()
        .map_err(|_| "solo pool state poisoned".to_owned())?;
    if !ready {
        if services.pool.is_some() {
            eprintln!("kraskus solo pool: Solo Prover Ready lost; closing the miner endpoint");
            services.stop().map_err(|e| e.to_string())?;
        }
        return Ok(None);
    }
    if let Some(pool) = &services.pool {
        return Ok(Some(pool.local_addr().to_string()));
    }
    let mut config = solo_pool_config(
        s.endpoint.bind,
        s.endpoint.certificate_der.clone(),
        s.endpoint.private_key_der.clone(),
        s.endpoint.destination,
    );
    let prover = s.prover.as_ref().ok_or(NO_PROVER)?;
    config.production_v4_share_verifier = Some(Arc::new(RemoteVerifier(Arc::clone(prover))));
    let pool = spawn_pool_server(Arc::clone(&s.node), config).map_err(|e| e.to_string())?;
    let dashboard = match (
        s.args.solo_pool_dashboard_bind,
        s.args.solo_pool_dashboard_assets.as_ref(),
        s.args.solo_pool_public_url.as_ref(),
    ) {
        (Some(bind), Some(assets), Some(url)) => Some(
            spawn_pool_dashboard(
                pool.dashboard_source(),
                PoolDashboardConfig {
                    bind,
                    assets_directory: assets.clone(),
                    public_pool_url: url.clone(),
                    certificate_sha256: s.endpoint.pin,
                },
            )
            .map_err(|e| e.to_string())?,
        ),
        _ => None,
    };
    let address = pool.local_addr().to_string();
    eprintln!("kraskus solo pool: Solo Prover Ready; miner endpoint open on {address}");
    services.pool = Some(pool);
    services.dashboard = dashboard;
    Ok(Some(address))
}

fn write_snapshot(s: &Supervisor) {
    let Some(path) = &s.args.solo_pool_snapshot_file else {
        return;
    };
    let snapshot = s
        .services
        .lock()
        .ok()
        .and_then(|services| services.pool.as_ref().map(|p| p.dashboard_source()))
        .and_then(|source| source.snapshot().ok())
        .and_then(|snapshot| serde_json::to_value(snapshot).ok());
    match snapshot {
        Some(value) => {
            if let Err(e) = write_atomically(path, &value) {
                eprintln!("kraskus solo pool: snapshot not written: {e}");
            }
        }
        // No endpoint, no snapshot: a stale file must never look live.
        None => {
            let _ = fs::remove_file(path);
        }
    }
}

/// The solo configuration of the unmodified upstream pool server.
pub fn solo_pool_config(
    bind: SocketAddr,
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
    destination: [u8; 32],
) -> PoolServerConfig {
    let mut config = PoolServerConfig::devnet(bind, certificate_der, private_key_der, destination);
    config.share_target = SOLO_SHARE_TARGET;
    config.test_credit_atoms_per_share = 0;
    config.ledger_directory = None;
    config.payout_policy = None;
    config.pplns_policy = None;
    config.allow_public_clients = false;
    config.allow_address_only_payouts = false;
    config
}

#[derive(Clone)]
struct StatusWriter {
    path: Option<PathBuf>,
    document: Arc<Mutex<Value>>,
}

impl StatusWriter {
    fn new(path: Option<PathBuf>, base: Value) -> Self {
        Self {
            path,
            document: Arc::new(Mutex::new(base)),
        }
    }

    fn current(&self) -> Value {
        self.document
            .lock()
            .map(|document| document.clone())
            .unwrap_or(Value::Null)
    }

    fn set(&self, key: &str, value: Value) {
        if let Ok(mut document) = self.document.lock()
            && let Some(object) = document.as_object_mut()
        {
            object.insert(key.to_owned(), value);
        }
    }

    fn update(
        &self,
        state: &str,
        error: Option<String>,
        detail: Option<Value>,
        prover: Option<Value>,
    ) {
        let Ok(mut document) = self.document.lock() else {
            return;
        };
        let object = document
            .as_object_mut()
            .expect("status document is an object");
        object.insert("state".to_owned(), json!(state));
        object.insert("error".to_owned(), json!(error));
        object.insert("detail".to_owned(), detail.unwrap_or(Value::Null));
        if let Some(prover) = prover {
            object.insert("prover".to_owned(), prover);
        }
        object.insert(
            "updated_unix".to_owned(),
            json!(unix_time_seconds().unwrap_or(0)),
        );
        if let Some(path) = &self.path
            && let Err(error) = write_atomically(path, &document)
        {
            eprintln!("kraskus solo pool: status file not written: {error}");
        }
    }
}

fn write_atomically(path: &Path, document: &Value) -> Result<(), Box<dyn Error>> {
    let temporary = path.with_extension("json.tmp");
    {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(document)?)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cmfd_consensus::forgematrix::target_with_leading_zero_bits;

    fn config() -> PoolServerConfig {
        solo_pool_config("127.0.0.1:0".parse().unwrap(), vec![1], vec![2], [7; 32])
    }

    #[test]
    fn solo_config_has_no_payout_accounting() {
        let config = config();
        assert!(config.payout_policy.is_none());
        assert!(config.pplns_policy.is_none());
        assert!(config.ledger_directory.is_none());
        assert_eq!(config.test_credit_atoms_per_share, 0);
        assert!(!config.allow_public_clients);
        assert!(!config.allow_address_only_payouts);
        assert_eq!(config.block_destination, [7; 32]);
    }

    #[test]
    fn solo_share_target_resolves_to_the_chain_target() {
        // spawn_pool_server takes max(configured, chain): the easier target.
        let config = config();
        for bits in [0_u16, 8, 20, 64, 255] {
            let chain = target_with_leading_zero_bits(bits);
            assert_eq!(config.share_target.max(chain), chain);
        }
    }

    #[test]
    fn prover_pairing_file_accepts_plain_or_json_and_fails_closed() {
        let dir = std::env::temp_dir().join(format!("kraskus-solo-pairing-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("prover.json");
        let source = ProverSource::File(path.clone());
        assert_eq!(source.url().unwrap_err(), NO_PROVER, "missing file");
        fs::write(&path, "  \n").unwrap();
        assert_eq!(source.url().unwrap_err(), NO_PROVER, "empty file");
        fs::write(&path, r#"{"url": ""}"#).unwrap();
        assert_eq!(source.url().unwrap_err(), NO_PROVER, "empty url");
        let url = format!(
            "cmfd-prover+tls://192.168.1.50:29460?pin={}",
            "ab".repeat(32)
        );
        fs::write(&path, format!("{url}\n")).unwrap();
        assert_eq!(source.url().unwrap(), url);
        fs::write(&path, serde_json::to_vec(&json!({"url": url})).unwrap()).unwrap();
        assert_eq!(source.url().unwrap(), url);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn status_file_is_written_atomically() {
        let directory = std::env::temp_dir().join(format!(
            "kraskus-solo-status-{}-{}",
            std::process::id(),
            unix_time_seconds().unwrap()
        ));
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("solo.json");
        let writer = StatusWriter::new(Some(path.clone()), json!({"schema": SOLO_STATUS_SCHEMA}));
        writer.update(
            "prover_unavailable",
            Some("no GPU".to_owned()),
            None,
            Some(json!({"connected": false})),
        );
        let value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(value["state"], "prover_unavailable");
        assert_eq!(value["error"], "no GPU");
        assert_eq!(value["schema"], SOLO_STATUS_SCHEMA);
        assert!(!path.with_extension("json.tmp").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    /// End to end on the in-process DevNet reference profile: the unmodified
    /// pool client mines through the solo configuration, every share it can
    /// submit is a block, the block is accepted, the reward pays the node
    /// wallet and no payout ledger is written.
    #[test]
    fn solo_endpoint_mines_a_devnet_block_to_the_node_wallet() {
        use cmfd_node::DEVNET_PROFILE;
        use cmfd_node::pool::{
            PoolClient, PoolClientConfig, PoolPayoutSigner, PoolWorkSearchResult,
            generate_pool_certificate,
        };
        use k256::schnorr::SigningKey;

        let mut id = [0_u8; 16];
        getrandom::fill(&mut id).unwrap();
        let root = std::env::temp_dir().join(format!("kraskus-solo-e2e-{}", hex::encode(id)));
        fs::create_dir(&root).unwrap();
        let data = root.join("node");
        let node = Arc::new(Mutex::new(
            Node::open_with_profile(&data, DEVNET_PROFILE).unwrap(),
        ));
        let destination = node.lock().unwrap().wallet_destination();
        let immature_before = node
            .lock()
            .unwrap()
            .wallet_snapshot()
            .unwrap()
            .immature_utxo_count;
        let certificate = root.join("solo.crt.der");
        let key = root.join("solo.key.der");
        let pin = generate_pool_certificate(&certificate, &key)
            .unwrap()
            .certificate_sha256;
        let server = spawn_pool_server(
            Arc::clone(&node),
            solo_pool_config(
                "127.0.0.1:0".parse().unwrap(),
                fs::read(&certificate).unwrap(),
                fs::read(&key).unwrap(),
                destination,
            ),
        )
        .unwrap();

        let signer = PoolPayoutSigner::new(SigningKey::from_bytes(&[0x13; 32]).unwrap());
        let mut client = PoolClient::connect(
            PoolClientConfig::devnet(server.local_addr(), pin, "solo-e2e", signer).unwrap(),
        )
        .unwrap();
        let work = client.current_work().unwrap();
        assert_eq!(work.job().share_target, work.job().challenge.target);
        let height = work.job().challenge.height;
        let mut next = 0;
        let nonce = loop {
            match work.search_range(next, 1_000, || false).unwrap() {
                PoolWorkSearchResult::Found {
                    nonce,
                    meets_chain_target,
                    ..
                } => {
                    assert!(meets_chain_target, "solo shares are blocks only");
                    break nonce;
                }
                PoolWorkSearchResult::Exhausted { next_nonce, .. } => next = next_nonce,
                PoolWorkSearchResult::Cancelled { .. } => unreachable!(),
            }
        };
        let result = client.submit_share(work.job().job_id, nonce).unwrap();
        assert!(result.accepted);
        assert!(result.block_accepted);
        assert_eq!(result.code, "block_accepted");
        {
            let mut node = node.lock().unwrap();
            assert_eq!(node.status().unwrap().accepted_height, height);
            assert!(node.wallet_snapshot().unwrap().immature_utxo_count > immature_before);
        }
        drop(client);
        server.stop().unwrap();
        assert!(!data.join("pool-ledger").exists());
        drop(node);
        let _ = fs::remove_dir_all(root);
    }
}
