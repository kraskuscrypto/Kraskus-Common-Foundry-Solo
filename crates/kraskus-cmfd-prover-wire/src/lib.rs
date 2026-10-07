//! Kraskus remote LAN prover protocol (Kraskus-Common-Foundry-Solo; NOT upstream).
//!
//! The node keeps the wallet, chain, templates, consensus verification and block
//! submission. The prover only answers the upstream
//! `ProductionV4PoolShareVerifier::evaluate(template, nonce, share_target)` call.
//! This crate is the boundary between the two:
//!
//! - TLS 1.3 only, with **mutual exact SHA-256 certificate pinning**;
//! - numeric private (LAN) or loopback addresses only, on both ends;
//! - one length-bounded JSON header per message plus bounded binary blobs;
//! - transactions and proofs travel in the **upstream consensus wire codec**
//!   (`encode_transaction`, `encode_forgematrix_proof`), challenge and coinbase as
//!   the same serde form upstream writes for its own proof worker.
//!
//! Nothing here carries wallet keys, seeds, passphrases, chain data or proving data.

use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use cmfd_consensus::{
    BlockChallenge, BlockProof, Coinbase, Transaction, decode_forgematrix_proof,
    decode_transaction, encode_forgematrix_proof, encode_transaction,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, ClientConnection, DigitallySignedStruct, DistinguishedName, ServerConfig,
    ServerConnection, SignatureScheme, StreamOwned,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub use rustls;

pub const PROTOCOL: &str = "KRASKUS_CMFD_PROVER_V1";
pub const URL_SCHEME: &str = "cmfd-prover+tls";
pub const DEFAULT_PORT: u16 = 29460;
pub const SERVER_NAME: &str = "cmfd-prover.local";
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
pub const MAX_BLOBS: usize = 65_536;
/// Node → prover: the template's encoded transactions (blocks are ≤ 16 MiB).
pub const MAX_REQUEST_BLOB_BYTES: usize = 32 * 1024 * 1024;
/// Prover → node: one encoded proof (≤ 13 MiB upstream) plus framing.
pub const MAX_RESPONSE_BLOB_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    pub code: &'static str,
    pub message: String,
}

impl WireError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for WireError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for WireError {}

impl From<io::Error> for WireError {
    fn from(error: io::Error) -> Self {
        Self::new("io", error.to_string())
    }
}

impl From<serde_json::Error> for WireError {
    fn from(error: serde_json::Error) -> Self {
        Self::new("json", error.to_string())
    }
}

impl From<rustls::Error> for WireError {
    fn from(error: rustls::Error) -> Self {
        Self::new("tls", error.to_string())
    }
}

// ------------------------------------------------------------------ messages

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Hello {
        protocol: String,
        network_id: String,
        node_version: String,
    },
    Status,
    /// Blobs: the template's transactions, each in the upstream wire codec.
    Evaluate {
        request_id: u64,
        challenge: BlockChallenge,
        coinbase: Coinbase,
        total_fees_burned: u64,
        transactions: usize,
        nonce: u64,
        share_target: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Response {
    HelloAck {
        protocol: String,
        network_id: String,
        capability: Capability,
    },
    Status {
        capability: Capability,
    },
    /// Blob: exactly one upstream-encoded proof when `proof` is true.
    Evaluation {
        request_id: u64,
        work_digest: String,
        proof: bool,
        seconds: f64,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GpuInfo {
    pub name: String,
    pub uuid: Option<String>,
    pub compute_capability: Option<String>,
    pub vram_total_mib: Option<u64>,
    pub vram_free_mib: Option<u64>,
    pub driver_version: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileCheck {
    pub name: String,
    pub bytes: u64,
    pub expected_bytes: u64,
    pub ok: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DataStatus {
    /// True only after every catalog file matched its exact size and SHA-256.
    pub verified: bool,
    pub bytes: u64,
    pub expected_bytes: u64,
    pub checked_unix: Option<u64>,
    pub detail: Option<String>,
    pub files: Vec<FileCheck>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkerStatus {
    /// Both official workers started and printed their READY markers.
    pub ready: bool,
    /// Both worker binaries match their pinned SHA-256.
    pub pins_ok: bool,
    pub replay_sha256: Option<String>,
    pub proof_sha256: Option<String>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EvaluationRecord {
    pub at_unix: u64,
    pub ok: bool,
    pub seconds: f64,
    pub proved: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Capability {
    pub prover_version: String,
    pub upstream_commit: String,
    pub network_id: String,
    pub gpu: Option<GpuInfo>,
    pub proving_data: DataStatus,
    pub workers: WorkerStatus,
    pub busy: bool,
    pub last_evaluation: Option<EvaluationRecord>,
    pub reported_unix: u64,
}

// ------------------------------------------------------------ hex helpers

pub fn hex32(value: &[u8; 32]) -> String {
    hex::encode(value)
}

pub fn parse_hex32(value: &str) -> Result<[u8; 32], WireError> {
    let bytes =
        hex::decode(value).map_err(|_| WireError::new("hex", "expected 64 hex characters"))?;
    bytes
        .try_into()
        .map_err(|_| WireError::new("hex", "expected 64 hex characters"))
}

pub fn certificate_sha256(certificate_der: &[u8]) -> [u8; 32] {
    Sha256::digest(certificate_der).into()
}

// ------------------------------------------------------------- template/proof

pub fn encode_transactions(transactions: &[Transaction]) -> Result<Vec<Vec<u8>>, WireError> {
    transactions
        .iter()
        .map(|transaction| {
            encode_transaction(transaction)
                .map_err(|error| WireError::new("codec", error.to_string()))
        })
        .collect()
}

pub fn decode_transactions(
    blobs: &[Vec<u8>],
    network_id: [u8; 32],
) -> Result<Vec<Transaction>, WireError> {
    blobs
        .iter()
        .map(|blob| {
            decode_transaction(blob, network_id)
                .map_err(|error| WireError::new("codec", error.to_string()))
        })
        .collect()
}

pub fn encode_proof(proof: &BlockProof, network_id: [u8; 32]) -> Result<Vec<u8>, WireError> {
    encode_forgematrix_proof(proof, network_id)
        .map_err(|error| WireError::new("codec", error.to_string()))
}

pub fn decode_proof(bytes: &[u8], network_id: [u8; 32]) -> Result<BlockProof, WireError> {
    decode_forgematrix_proof(bytes, network_id)
        .map_err(|error| WireError::new("codec", error.to_string()))
}

// --------------------------------------------------------------- addresses

/// The same rule as upstream `validate_private_address`: loopback, RFC 1918 or IPv6 ULA.
pub fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_loopback() || ip.is_private(),
        IpAddr::V6(ip) => ip.is_loopback() || (ip.segments()[0] & 0xfe00) == 0xfc00,
    }
}

pub fn require_private(address: SocketAddr) -> Result<(), WireError> {
    if is_private(address.ip()) {
        Ok(())
    } else {
        Err(WireError::new(
            "public_address",
            format!("{address} is not a private (LAN) or loopback address"),
        ))
    }
}

/// `cmfd-prover+tls://<numeric private IP>:<port>?pin=<64 hex>`
pub fn parse_url(url: &str) -> Result<(SocketAddr, [u8; 32]), WireError> {
    let rest = url
        .strip_prefix(URL_SCHEME)
        .and_then(|rest| rest.strip_prefix("://"))
        .ok_or_else(|| {
            WireError::new("url", format!("prover URL must start with {URL_SCHEME}://"))
        })?;
    let (address, query) = rest
        .split_once("?pin=")
        .ok_or_else(|| WireError::new("url", "prover URL must end with ?pin=<64 hex>"))?;
    let address: SocketAddr = address
        .parse()
        .map_err(|_| WireError::new("url", "prover address must be a numeric IP and port"))?;
    require_private(address)?;
    Ok((address, parse_hex32(query)?))
}

pub fn format_url(address: SocketAddr, pin: [u8; 32]) -> String {
    format!("{URL_SCHEME}://{address}?pin={}", hex32(&pin))
}

// ---------------------------------------------------------------- framing

fn read_u32<R: Read>(reader: &mut R) -> Result<u32, WireError> {
    let mut bytes = [0_u8; 4];
    match reader.read_exact(&mut bytes) {
        Ok(()) => Ok(u32::from_be_bytes(bytes)),
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
            Err(WireError::new("closed", "connection closed"))
        }
        Err(error) => Err(error.into()),
    }
}

/// `u32 header length | header JSON | u32 blob count | (u32 length | bytes)*`
pub fn write_message<W: Write, T: Serialize>(
    writer: &mut W,
    header: &T,
    blobs: &[Vec<u8>],
    max_blob_bytes: usize,
) -> Result<(), WireError> {
    let body = serde_json::to_vec(header)?;
    if body.is_empty() || body.len() > MAX_HEADER_BYTES {
        return Err(WireError::new(
            "frame_limit",
            "header exceeds the frame limit",
        ));
    }
    let total: usize = blobs.iter().map(Vec::len).sum();
    if blobs.len() > MAX_BLOBS || total > max_blob_bytes {
        return Err(WireError::new(
            "frame_limit",
            "blobs exceed the frame limit",
        ));
    }
    writer.write_all(&(body.len() as u32).to_be_bytes())?;
    writer.write_all(&body)?;
    writer.write_all(&(blobs.len() as u32).to_be_bytes())?;
    for blob in blobs {
        writer.write_all(&(blob.len() as u32).to_be_bytes())?;
        writer.write_all(blob)?;
    }
    writer.flush()?;
    Ok(())
}

pub fn read_message<R: Read, T: DeserializeOwned>(
    reader: &mut R,
    max_blob_bytes: usize,
) -> Result<(T, Vec<Vec<u8>>), WireError> {
    let length = read_u32(reader)? as usize;
    if length == 0 || length > MAX_HEADER_BYTES {
        return Err(WireError::new(
            "frame_limit",
            "header exceeds the frame limit",
        ));
    }
    let mut body = vec![0_u8; length];
    reader.read_exact(&mut body)?;
    let header = serde_json::from_slice(&body)?;
    let count = read_u32(reader)? as usize;
    if count > MAX_BLOBS {
        return Err(WireError::new("frame_limit", "too many blobs"));
    }
    let mut remaining = max_blob_bytes;
    let mut blobs = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let length = read_u32(reader)? as usize;
        if length > remaining {
            return Err(WireError::new(
                "frame_limit",
                "blobs exceed the frame limit",
            ));
        }
        remaining -= length;
        let mut blob = vec![0_u8; length];
        reader.read_exact(&mut blob)?;
        blobs.push(blob);
    }
    Ok((header, blobs))
}

// ------------------------------------------------------------------- TLS

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
struct PinnedServer {
    pin: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if certificate_sha256(end_entity.as_ref()) != self.pin {
            return Err(rustls::Error::General(
                "prover certificate SHA-256 pin mismatch".to_owned(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedClients {
    pins: Vec<[u8; 32]>,
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for PinnedClients {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        if !self.pins.contains(&certificate_sha256(end_entity.as_ref())) {
            return Err(rustls::Error::General(
                "node client certificate is not on the prover's allow list".to_owned(),
            ));
        }
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signed,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Prover side: TLS 1.3, its own certificate, and only allow-listed node certificates.
pub fn server_config(
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
    allowed_node_pins: Vec<[u8; 32]>,
) -> Result<Arc<ServerConfig>, WireError> {
    if allowed_node_pins.is_empty() {
        return Err(WireError::new(
            "config",
            "at least one node certificate pin is required",
        ));
    }
    let provider = provider();
    let verifier = Arc::new(PinnedClients {
        pins: allowed_node_pins,
        provider: Arc::clone(&provider),
    });
    let config = ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(certificate_der)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_der)),
        )?;
    Ok(Arc::new(config))
}

/// Node side: TLS 1.3, the exact prover pin, and the node's own client certificate.
pub fn client_config(
    prover_pin: [u8; 32],
    certificate_der: Vec<u8>,
    private_key_der: Vec<u8>,
) -> Result<Arc<ClientConfig>, WireError> {
    let provider = provider();
    let verifier = Arc::new(PinnedServer {
        pin: prover_pin,
        provider: Arc::clone(&provider),
    });
    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(
            vec![CertificateDer::from(certificate_der)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key_der)),
        )?;
    Ok(Arc::new(config))
}

pub type ClientStream = StreamOwned<ClientConnection, TcpStream>;
pub type ServerStream = StreamOwned<ServerConnection, TcpStream>;

/// Connects to a private address and completes the mutually pinned handshake.
pub fn connect(
    address: SocketAddr,
    config: Arc<ClientConfig>,
    timeout: Duration,
) -> Result<ClientStream, WireError> {
    require_private(address)?;
    let socket = TcpStream::connect_timeout(&address, timeout)?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    let name = ServerName::try_from(SERVER_NAME).expect("static server name");
    let mut connection = ClientConnection::new(config, name)?;
    let mut socket = socket;
    while connection.is_handshaking() {
        connection.complete_io(&mut socket)?;
    }
    Ok(StreamOwned::new(connection, socket))
}

/// Completes the server half of the handshake for an accepted private-address peer.
pub fn accept(
    socket: TcpStream,
    config: Arc<ServerConfig>,
    timeout: Duration,
) -> Result<ServerStream, WireError> {
    require_private(socket.peer_addr()?)?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(timeout))?;
    socket.set_write_timeout(Some(timeout))?;
    let mut connection = ServerConnection::new(config)?;
    let mut socket = socket;
    while connection.is_handshaking() {
        connection.complete_io(&mut socket)?;
    }
    Ok(StreamOwned::new(connection, socket))
}

/// Sets the read deadline for the next response (an evaluation may take minutes).
pub fn set_read_timeout(stream: &ClientStream, timeout: Duration) -> Result<(), WireError> {
    stream.sock.set_read_timeout(Some(timeout))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::thread;

    fn identity() -> (Vec<u8>, Vec<u8>) {
        let generated = rcgen::generate_simple_self_signed(vec![SERVER_NAME.to_owned()]).unwrap();
        (
            generated.cert.der().to_vec(),
            generated.signing_key.serialize_der(),
        )
    }

    /// Serves one connection: echoes a Status request with an empty capability.
    fn serve_once(
        server: Arc<ServerConfig>,
    ) -> (SocketAddr, thread::JoinHandle<Result<(), WireError>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (socket, _) = listener.accept()?;
            let mut stream = accept(socket, server, Duration::from_secs(5))?;
            let (request, _): (Request, _) = read_message(&mut stream, MAX_REQUEST_BLOB_BYTES)?;
            assert_eq!(request, Request::Status);
            write_message(
                &mut stream,
                &Response::Status {
                    capability: Capability::default(),
                },
                &[vec![1, 2, 3]],
                MAX_RESPONSE_BLOB_BYTES,
            )
        });
        (address, handle)
    }

    #[test]
    fn mutually_pinned_tls13_roundtrip() {
        let (server_cert, server_key) = identity();
        let (node_cert, node_key) = identity();
        let server = server_config(
            server_cert.clone(),
            server_key,
            vec![certificate_sha256(&node_cert)],
        )
        .unwrap();
        let (address, handle) = serve_once(server);
        let client = client_config(certificate_sha256(&server_cert), node_cert, node_key).unwrap();
        let mut stream = connect(address, client, Duration::from_secs(5)).unwrap();
        assert_eq!(
            stream.conn.protocol_version(),
            Some(rustls::ProtocolVersion::TLSv1_3)
        );
        write_message(&mut stream, &Request::Status, &[], MAX_REQUEST_BLOB_BYTES).unwrap();
        let (response, blobs): (Response, _) =
            read_message(&mut stream, MAX_RESPONSE_BLOB_BYTES).unwrap();
        assert!(matches!(response, Response::Status { .. }));
        assert_eq!(blobs, vec![vec![1, 2, 3]]);
        handle.join().unwrap().unwrap();
    }

    #[test]
    fn wrong_prover_pin_is_refused_by_the_node() {
        let (server_cert, server_key) = identity();
        let (node_cert, node_key) = identity();
        let server = server_config(
            server_cert,
            server_key,
            vec![certificate_sha256(&node_cert)],
        )
        .unwrap();
        let (address, handle) = serve_once(server);
        let client = client_config([7; 32], node_cert, node_key).unwrap();
        assert!(connect(address, client, Duration::from_secs(5)).is_err());
        assert!(handle.join().unwrap().is_err());
    }

    #[test]
    fn unknown_node_certificate_is_refused_by_the_prover() {
        let (server_cert, server_key) = identity();
        let (node_cert, node_key) = identity();
        let server = server_config(server_cert.clone(), server_key, vec![[9; 32]]).unwrap();
        let (address, handle) = serve_once(server);
        let client = client_config(certificate_sha256(&server_cert), node_cert, node_key).unwrap();
        // The client may finish its half before the alert arrives; any use must fail.
        let result = connect(address, client, Duration::from_secs(5)).and_then(|mut stream| {
            write_message(&mut stream, &Request::Status, &[], MAX_REQUEST_BLOB_BYTES)?;
            read_message::<_, Response>(&mut stream, MAX_RESPONSE_BLOB_BYTES).map(|_| ())
        });
        assert!(result.is_err());
        assert!(handle.join().unwrap().is_err());
    }

    #[test]
    fn server_requires_a_node_pin() {
        let (server_cert, server_key) = identity();
        assert_eq!(
            server_config(server_cert, server_key, vec![])
                .unwrap_err()
                .code,
            "config"
        );
    }

    #[test]
    fn public_addresses_and_bad_urls_are_refused() {
        assert!(require_private("8.8.8.8:29460".parse().unwrap()).is_err());
        assert!(require_private("192.168.1.50:29460".parse().unwrap()).is_ok());
        assert!(require_private("[fd00::1]:29460".parse().unwrap()).is_ok());
        let pin = "ab".repeat(32);
        assert!(parse_url(&format!("cmfd-prover+tls://8.8.8.8:29460?pin={pin}")).is_err());
        assert!(parse_url(&format!("cmfd-prover+tls://prover.local:29460?pin={pin}")).is_err());
        assert!(parse_url("cmfd-prover+tls://192.168.1.50:29460?pin=abc").is_err());
        assert!(parse_url(&format!("cmfd+tls://192.168.1.50:29460?pin={pin}")).is_err());
        let (address, parsed) =
            parse_url(&format!("cmfd-prover+tls://192.168.1.50:29460?pin={pin}")).unwrap();
        assert_eq!(address, "192.168.1.50:29460".parse().unwrap());
        assert_eq!(
            format_url(address, parsed),
            format!("cmfd-prover+tls://192.168.1.50:29460?pin={pin}")
        );
    }

    #[test]
    fn oversized_frames_are_refused_both_ways() {
        let mut buffer = Vec::new();
        assert!(write_message(&mut buffer, &Request::Status, &[vec![0; 11]], 10).is_err());
        write_message(&mut buffer, &Request::Status, &[vec![0; 10]], 10).unwrap();
        assert!(read_message::<_, Request>(&mut buffer.as_slice(), 9).is_err());
        assert!(read_message::<_, Request>(&mut buffer.as_slice(), 10).is_ok());
        let mut huge = Vec::from(((MAX_HEADER_BYTES + 1) as u32).to_be_bytes());
        huge.extend(vec![b' '; MAX_HEADER_BYTES + 1]);
        assert_eq!(
            read_message::<_, Request>(&mut huge.as_slice(), 10)
                .unwrap_err()
                .code,
            "frame_limit"
        );
        let mut many = Vec::new();
        let header = serde_json::to_vec(&Request::Status).unwrap();
        many.extend((header.len() as u32).to_be_bytes());
        many.extend(&header);
        many.extend(((MAX_BLOBS + 1) as u32).to_be_bytes());
        assert_eq!(
            read_message::<_, Request>(&mut many.as_slice(), 10)
                .unwrap_err()
                .code,
            "frame_limit"
        );
    }

    #[test]
    fn unknown_message_types_are_rejected() {
        let mut buffer = Vec::new();
        write_message(
            &mut buffer,
            &serde_json::json!({"type": "shell", "command": "id"}),
            &[],
            0,
        )
        .unwrap();
        assert!(read_message::<_, Request>(&mut buffer.as_slice(), 0).is_err());
    }
}
