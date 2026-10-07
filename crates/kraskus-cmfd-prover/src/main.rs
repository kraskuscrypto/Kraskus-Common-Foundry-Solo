//! `kraskus-cmfd-prover`: remote LAN prover service (Kraskus-Common-Foundry-Solo).

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use clap::{Parser, Subcommand};
use kraskus_cmfd_prover::{
    NvidiaSmi, Prover, ProverIdentity, VERSION, check_worker_pins, verify_proving_data,
};
use kraskus_cmfd_prover_wire as wire;

/// Upstream commit of the official workers this service drives.
const UPSTREAM_COMMIT: &str = "3aa5369512f47d0b3c49a49a54a0395e71e0c000";
const RETRY: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(name = "kraskus-cmfd-prover", version = VERSION,
          about = "Kraskus remote LAN prover for Common Foundry solo mining (experimental)")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)] // parsed once at start-up
enum Cmd {
    /// Create the prover's TLS identity (DER certificate, DER PKCS#8 key). Never overwrites.
    Certificate {
        #[arg(long)]
        certificate: PathBuf,
        #[arg(long)]
        private_key: PathBuf,
    },
    /// Serve evaluations to allow-listed nodes on the LAN.
    Serve {
        /// Private (LAN) or loopback address to listen on.
        #[arg(long)]
        bind: SocketAddr,
        #[arg(long)]
        certificate: PathBuf,
        #[arg(long)]
        private_key: PathBuf,
        /// SHA-256 of a node's client certificate allowed to connect (repeatable).
        #[arg(long = "allow-node-pin", required = true)]
        allow_node_pins: Vec<String>,
        /// Extracted official signed runtime package (production-v4/, lib/, catalog).
        #[arg(long)]
        runtime_dir: PathBuf,
        /// Official artifact catalog; defaults to <runtime>/production-v4-rcnet-1-inputs.json
        /// (the catalog upstream's mainnet pool service verifies).
        #[arg(long)]
        input_catalog: Option<PathBuf>,
        #[arg(long)]
        expected_replay_sha256: String,
        #[arg(long)]
        expected_proof_sha256: String,
        /// Absolute scratch directory for one attempt's replay intermediates.
        #[arg(long)]
        scratch: PathBuf,
        /// Directory for the proving-data verification cache.
        #[arg(long)]
        state_dir: PathBuf,
        #[arg(long)]
        gpu_uuid: Option<String>,
        #[arg(long, default_value = "nvidia-smi")]
        nvidia_smi: PathBuf,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Cmd::Certificate {
            certificate,
            private_key,
        } => {
            let info = cmfd_node::pool::generate_pool_certificate(&certificate, &private_key)?;
            println!(
                "{}",
                serde_json::json!({
                    "certificate": info.certificate_path,
                    "private_key": info.private_key_path,
                    "certificate_sha256": hex::encode(info.certificate_sha256),
                })
            );
            Ok(())
        }
        Cmd::Serve {
            bind,
            certificate,
            private_key,
            allow_node_pins,
            runtime_dir,
            input_catalog,
            expected_replay_sha256,
            expected_proof_sha256,
            scratch,
            state_dir,
            gpu_uuid,
            nvidia_smi,
        } => {
            wire::require_private(bind)?;
            let pins = allow_node_pins
                .iter()
                .map(|p| wire::parse_hex32(p))
                .collect::<Result<Vec<_>, _>>()?;
            let certificate_der = std::fs::read(&certificate)?;
            let tls =
                wire::server_config(certificate_der.clone(), std::fs::read(&private_key)?, pins)?;
            std::fs::create_dir_all(&state_dir)?;
            let prover = Prover::new(
                ProverIdentity {
                    network_id: cmfd_node::COMPILED_NETWORK_PROFILE.network_id,
                    upstream_commit: UPSTREAM_COMMIT.to_owned(),
                },
                Box::new(NvidiaSmi {
                    program: nvidia_smi,
                    uuid: gpu_uuid.clone(),
                }),
            );
            let listener = TcpListener::bind(bind)?;
            let address = listener.local_addr()?;
            println!(
                "{}",
                serde_json::json!({
                    "prover": wire::format_url(address, wire::certificate_sha256(&certificate_der)),
                    "version": VERSION,
                    "network": cmfd_node::COMPILED_NETWORK_PROFILE.short_name(),
                })
            );
            let catalog = input_catalog
                .unwrap_or_else(|| runtime_dir.join("production-v4-rcnet-1-inputs.json"));
            {
                let prover = Arc::clone(&prover);
                thread::Builder::new()
                    .name("kraskus-prover-prepare".to_owned())
                    .spawn(move || {
                        let dir = runtime_dir.join("production-v4");
                        loop {
                            if prover.workers().ready {
                                thread::sleep(Duration::from_secs(5));
                                continue;
                            }
                            let workers = check_worker_pins(
                                &dir.join("cmfd-v4-replay"),
                                &dir.join("real_bank0_relations"),
                                &expected_replay_sha256,
                                &expected_proof_sha256,
                            );
                            let pins_ok = workers.pins_ok;
                            prover.set_workers(workers);
                            let data = verify_proving_data(
                                &runtime_dir,
                                &catalog,
                                Some(&state_dir.join("proving-data-verification.json")),
                                |progress| prover.set_data(progress.clone()),
                            );
                            let data_ok = data.verified;
                            prover.set_data(data);
                            if pins_ok && data_ok {
                                match start(&runtime_dir, &scratch, gpu_uuid.as_deref()) {
                                    Ok(verifier) => {
                                        prover.install_verifier(verifier);
                                        eprintln!("kraskus prover: official workers READY");
                                        continue;
                                    }
                                    Err(error) => {
                                        eprintln!(
                                            "kraskus prover: workers failed to start: {error}"
                                        );
                                        prover.withdraw_verifier(error);
                                    }
                                }
                            }
                            thread::sleep(RETRY);
                        }
                    })?;
            }
            prover.serve(listener, tls, Arc::new(AtomicBool::new(false)));
            Ok(())
        }
    }
}

#[cfg(feature = "production-mainnet")]
fn start(
    runtime: &std::path::Path,
    scratch: &std::path::Path,
    gpu: Option<&str>,
) -> Result<Arc<dyn cmfd_node::pool::ProductionV4PoolShareVerifier>, String> {
    kraskus_cmfd_prover::start_official_verifier(runtime, scratch, gpu)
}

#[cfg(not(feature = "production-mainnet"))]
fn start(
    _runtime: &std::path::Path,
    _scratch: &std::path::Path,
    _gpu: Option<&str>,
) -> Result<Arc<dyn cmfd_node::pool::ProductionV4PoolShareVerifier>, String> {
    Err(
        "this build has no ProductionV4 workers (build with --features production-mainnet)"
            .to_owned(),
    )
}
