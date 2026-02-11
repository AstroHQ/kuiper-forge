//! Integration tests for gRPC and mTLS functionality.
//!
//! These tests verify that:
//! - gRPC works over TLS (ALPN h2 negotiation)
//! - Registration service works without client cert
//! - Agent service requires mTLS (rejects requests without client cert)
//! - Both gRPC-only and webhook modes work correctly
//! - PROXY protocol v1/v2 support works correctly

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kuiper_agent_proto::{
    AgentMessage, AgentPayload, AgentServiceClient, AgentStatus, RegisterRequest,
    RegistrationServiceClient,
};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tokio_rustls::TlsConnector;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};

// Install default crypto provider for rustls
fn install_crypto_provider() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    });
}

/// Test fixture that sets up certificates and server for testing
struct TestFixture {
    _temp_dir: TempDir,
    ca_cert_pem: String,
    server_addr: SocketAddr,
    /// Channel for signaling server shutdown
    shutdown_tx: Option<oneshot::Sender<()>>,
}

impl TestFixture {
    /// Create test CA, server cert, and client cert
    async fn new(webhook_mode: bool) -> Self {
        Self::new_with_proxy_protocol(webhook_mode, false).await
    }

    /// Create test fixture with optional proxy protocol support
    async fn new_with_proxy_protocol(webhook_mode: bool, proxy_protocol: bool) -> Self {
        use kuiper_forge::auth::{AuthManager, AuthStore, generate_server_cert, init_ca};
        use kuiper_forge::config::{DatabaseConfig, TlsConfig, WebhookConfig};
        use kuiper_forge::db::Database;
        use kuiper_forge::tls::build_server_trust;

        let temp_dir = TempDir::new().unwrap();
        let ca_cert_path = temp_dir.path().join("ca.crt");
        let ca_key_path = temp_dir.path().join("ca.key");
        let server_cert_path = temp_dir.path().join("server.crt");
        let server_key_path = temp_dir.path().join("server.key");

        // Initialize CA
        init_ca(&ca_cert_path, &ca_key_path, "Test Org").unwrap();

        // Generate server certificate for localhost
        generate_server_cert(
            &ca_cert_path,
            &ca_key_path,
            &server_cert_path,
            &server_key_path,
            "localhost",
        )
        .unwrap();

        let ca_cert_pem = std::fs::read_to_string(&ca_cert_path).unwrap();

        // Create database and auth components
        let db_config = DatabaseConfig::default();
        let db = Arc::new(Database::new(&db_config, temp_dir.path()).await.unwrap());
        let auth_store = Arc::new(AuthStore::new(db.pool().clone()));
        let auth_manager =
            Arc::new(AuthManager::new(auth_store.clone(), &ca_cert_path, &ca_key_path).unwrap());

        // Create agent registry
        let agent_registry = Arc::new(kuiper_forge::agent_registry::AgentRegistry::new());

        // Find available port
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let server_addr = listener.local_addr().unwrap();
        drop(listener);

        // Create TLS config
        let tls_config = TlsConfig {
            ca_cert: ca_cert_path.clone(),
            ca_key: ca_key_path.clone(),
            server_cert: server_cert_path.clone(),
            server_key: server_key_path.clone(),
            server_ca_cert: None,
            server_use_native_roots: false,
            server_trust_mode: kuiper_forge::config::ServerTrustMode::Ca,
        };

        // Build server trust before moving tls_config
        let server_trust = build_server_trust(&tls_config).unwrap();

        // Create server config
        let server_config = kuiper_forge::server::ServerConfig {
            listen_addr: server_addr,
            tls: tls_config,
            proxy_protocol,
        };

        // Create webhook config and pending job store if needed
        let (webhook_config, pending_job_store) = if webhook_mode {
            let (tx, _rx) = tokio::sync::mpsc::channel(32);
            let webhook_notifier = kuiper_forge::webhook::WebhookNotifier::new(tx);
            let wh_config = WebhookConfig {
                path: "/webhook".to_string(),
                secret: "test-secret".to_string(),
                required_labels: vec!["self-hosted".to_string()],
                label_mappings: vec![],
            };
            let pending_store =
                Arc::new(kuiper_forge::pending_jobs::PendingJobStore::new(db.pool()));
            (Some((wh_config, webhook_notifier)), Some(pending_store))
        } else {
            (None, None)
        };

        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // Spawn server in background
        let auth_manager_clone = auth_manager.clone();
        let agent_registry_clone = agent_registry.clone();
        tokio::spawn(async move {
            let server_fut = kuiper_forge::server::run_server(
                server_config,
                server_trust,
                auth_manager_clone,
                agent_registry_clone,
                None, // fleet_notifier
                None, // runner_state
                pending_job_store,
                webhook_config,
                None, // admin_state
            );

            tokio::select! {
                result = server_fut => {
                    if let Err(e) = result {
                        eprintln!("Server error: {e}");
                    }
                }
                _ = shutdown_rx => {
                    // Shutdown requested
                }
            }
        });

        // Give server time to start
        tokio::time::sleep(Duration::from_millis(100)).await;

        Self {
            _temp_dir: temp_dir,
            ca_cert_pem,
            server_addr,
            shutdown_tx: Some(shutdown_tx),
        }
    }

    /// Build a rustls client config for manual TLS connections
    fn build_tls_client_config(&self) -> Arc<rustls::ClientConfig> {
        let mut root_store = rustls::RootCertStore::empty();
        for cert in rustls_pemfile::certs(&mut self.ca_cert_pem.as_bytes()) {
            root_store.add(cert.unwrap()).unwrap();
        }

        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth(),
        )
    }

    /// Create a gRPC channel without client certificate (for registration)
    async fn channel_without_client_cert(&self) -> Channel {
        let tls_config = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(&self.ca_cert_pem))
            .domain_name("localhost");

        Channel::from_shared(format!("https://{}", self.server_addr))
            .unwrap()
            .tls_config(tls_config)
            .unwrap()
            .connect()
            .await
            .unwrap()
    }
}

impl Drop for TestFixture {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
    }
}

/// Test that gRPC connection works over TLS in gRPC-only mode (verifies ALPN h2 negotiation)
#[tokio::test]
async fn test_grpc_tls_connection_grpc_only_mode() {
    install_crypto_provider();
    let fixture = TestFixture::new(false).await;
    let channel = fixture.channel_without_client_cert().await;

    // Just connecting successfully proves TLS and ALPN work
    // The channel is established over HTTP/2
    let mut client = RegistrationServiceClient::new(channel);

    // Make a request - it should fail with invalid token, but the gRPC call itself works
    let result = client
        .register(RegisterRequest {
            registration_token: "invalid-token".to_string(),
            hostname: "test-host".to_string(),
            agent_type: "tart".to_string(),
            labels: vec!["test".to_string()],
            max_vms: 2,
        })
        .await;

    // Should get a gRPC error (not a connection error)
    assert!(result.is_err());
    let status = result.unwrap_err();
    // The error should be about the invalid token, not a connection issue
    assert_eq!(
        status.code(),
        tonic::Code::Unauthenticated,
        "Expected Unauthenticated, got {:?}: {}",
        status.code(),
        status.message()
    );
}

/// Test that agent service rejects requests without client certificate in gRPC-only mode
#[tokio::test]
async fn test_agent_service_requires_mtls_grpc_only_mode() {
    install_crypto_provider();
    let fixture = TestFixture::new(false).await;
    let channel = fixture.channel_without_client_cert().await;

    let mut client = AgentServiceClient::new(channel);

    // Try to call agent_stream without client cert
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(AgentMessage {
        payload: Some(AgentPayload::Status(AgentStatus {
            active_vms: 0,
            available_slots: 2,
            vms: vec![],
            agent_id: "test-agent".to_string(),
            hostname: "test-host".to_string(),
            agent_type: "tart".to_string(),
            labels: vec!["test".to_string()],
            max_vms: 2,
            label_sets: vec![],
        })),
    })
    .await
    .unwrap();
    drop(tx);

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let result = client.agent_stream(stream).await;

    // Should be rejected - no client cert
    assert!(result.is_err());
    let status = result.unwrap_err();
    assert_eq!(
        status.code(),
        tonic::Code::Unauthenticated,
        "Expected Unauthenticated, got {:?}: {}",
        status.code(),
        status.message()
    );
    assert!(
        status.message().contains("TLS")
            || status.message().contains("certificate")
            || status.message().contains("mTLS"),
        "Expected TLS/certificate error, got: {}",
        status.message()
    );
}

/// Test gRPC works in webhook mode (verifies ALPN fix for multiplexed server)
#[tokio::test]
async fn test_grpc_tls_connection_webhook_mode() {
    install_crypto_provider();
    let fixture = TestFixture::new(true).await;
    let channel = fixture.channel_without_client_cert().await;

    let mut client = RegistrationServiceClient::new(channel);

    let result = client
        .register(RegisterRequest {
            registration_token: "invalid-token".to_string(),
            hostname: "test-host".to_string(),
            agent_type: "tart".to_string(),
            labels: vec!["test".to_string()],
            max_vms: 2,
        })
        .await;

    // Should get a proper gRPC error, not a connection error
    assert!(result.is_err());
    let status = result.unwrap_err();
    assert_eq!(
        status.code(),
        tonic::Code::Unauthenticated,
        "Expected Unauthenticated (invalid token), got {:?}: {}",
        status.code(),
        status.message()
    );
}

/// Test that agent service requires mTLS in webhook mode (verifies PeerCertificates injection)
#[tokio::test]
async fn test_agent_service_requires_mtls_webhook_mode() {
    install_crypto_provider();
    let fixture = TestFixture::new(true).await;
    let channel = fixture.channel_without_client_cert().await;

    let mut client = AgentServiceClient::new(channel);

    // Try to call agent_stream without client cert
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(AgentMessage {
        payload: Some(AgentPayload::Status(AgentStatus {
            active_vms: 0,
            available_slots: 2,
            vms: vec![],
            agent_id: "test-agent".to_string(),
            hostname: "test-host".to_string(),
            agent_type: "tart".to_string(),
            labels: vec!["test".to_string()],
            max_vms: 2,
            label_sets: vec![],
        })),
    })
    .await
    .unwrap();
    drop(tx);

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let result = client.agent_stream(stream).await;

    // Should be rejected - no client cert
    assert!(result.is_err());
    let status = result.unwrap_err();
    assert_eq!(
        status.code(),
        tonic::Code::Unauthenticated,
        "Expected Unauthenticated, got {:?}: {}",
        status.code(),
        status.message()
    );
    assert!(
        status.message().contains("TLS")
            || status.message().contains("certificate")
            || status.message().contains("mTLS"),
        "Expected TLS/certificate error, got: {}",
        status.message()
    );
}

// ============================================================================
// PROXY Protocol Tests
// ============================================================================

/// Build a PROXY protocol v1 header for TCP4
fn build_proxy_v1_header(src_addr: &str, src_port: u16, dst_addr: &str, dst_port: u16) -> Vec<u8> {
    format!(
        "PROXY TCP4 {} {} {} {}\r\n",
        src_addr, dst_addr, src_port, dst_port
    )
    .into_bytes()
}

/// Build a PROXY protocol v2 header for TCP4
fn build_proxy_v2_header(
    src_addr: [u8; 4],
    src_port: u16,
    dst_addr: [u8; 4],
    dst_port: u16,
) -> Vec<u8> {
    // PROXY protocol v2 header format:
    // - 12 byte signature: \x0D\x0A\x0D\x0A\x00\x0D\x0A\x51\x55\x49\x54\x0A
    // - 1 byte: version (2) << 4 | command (1 = PROXY)
    // - 1 byte: address family (1 = AF_INET) << 4 | transport (1 = STREAM)
    // - 2 bytes: address length (big-endian)
    // - addresses: src_addr (4) + dst_addr (4) + src_port (2) + dst_port (2) = 12 bytes
    let mut header = Vec::with_capacity(28);

    // Signature (12 bytes)
    header.extend_from_slice(&[
        0x0D, 0x0A, 0x0D, 0x0A, 0x00, 0x0D, 0x0A, 0x51, 0x55, 0x49, 0x54, 0x0A,
    ]);

    // Version and command: (2 << 4) | 1 = 0x21
    header.push(0x21);

    // Address family and transport: (1 << 4) | 1 = 0x11 (AF_INET, STREAM)
    header.push(0x11);

    // Address length: 12 bytes (4+4+2+2)
    header.extend_from_slice(&[0x00, 0x0C]);

    // Source address
    header.extend_from_slice(&src_addr);
    // Destination address
    header.extend_from_slice(&dst_addr);
    // Source port (big-endian)
    header.extend_from_slice(&src_port.to_be_bytes());
    // Destination port (big-endian)
    header.extend_from_slice(&dst_port.to_be_bytes());

    header
}

/// Test that gRPC works with PROXY protocol v1 enabled
#[tokio::test]
async fn test_grpc_with_proxy_protocol_v1() {
    install_crypto_provider();

    // Start server with proxy protocol enabled
    let fixture = TestFixture::new_with_proxy_protocol(false, true).await;

    // Build PROXY protocol v1 header
    let proxy_header = build_proxy_v1_header("192.168.1.100", 12345, "10.0.0.1", 9443);

    // Connect via raw TCP, send PROXY header, then TLS handshake
    let mut tcp_stream = TcpStream::connect(fixture.server_addr).await.unwrap();

    // Send PROXY protocol header first and ensure it's fully sent
    tcp_stream.write_all(&proxy_header).await.unwrap();
    tcp_stream.flush().await.unwrap();

    // TLS handshake
    let tls_config = fixture.build_tls_client_config();
    let connector = TlsConnector::from(tls_config);
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls_stream = connector.connect(server_name, tcp_stream).await.unwrap();

    // Build HTTP/2 channel over the TLS stream
    let (h2_sender, h2_conn) = h2::client::handshake(tls_stream).await.unwrap();
    tokio::spawn(async move {
        if let Err(e) = h2_conn.await {
            eprintln!("HTTP/2 connection error: {}", e);
        }
    });

    // Make a gRPC request manually using h2
    let mut sender = h2_sender.ready().await.unwrap();

    let request = http::Request::builder()
        .method("POST")
        .uri(format!(
            "https://{}/kuiper_agent_proto.RegistrationService/Register",
            fixture.server_addr
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .unwrap();

    let (response, _send_stream) = sender.send_request(request, false).unwrap();
    let response = response.await.unwrap();

    // We expect the request to reach the server (connection established through PROXY protocol)
    // The response should be a gRPC error (unauthenticated) not a connection error
    assert!(
        response.status().is_success() || response.status() == http::StatusCode::OK,
        "Expected HTTP 200 (gRPC uses 200 even for errors), got {}",
        response.status()
    );
}

/// Test that gRPC works with PROXY protocol v2 enabled
#[tokio::test]
async fn test_grpc_with_proxy_protocol_v2() {
    install_crypto_provider();

    // Start server with proxy protocol enabled
    let fixture = TestFixture::new_with_proxy_protocol(false, true).await;

    // Build PROXY protocol v2 header (simulating client from 192.168.1.100:12345)
    let proxy_header = build_proxy_v2_header(
        [192, 168, 1, 100], // source IP
        12345,              // source port
        [10, 0, 0, 1],      // dest IP
        9443,               // dest port
    );

    // Connect via raw TCP
    let mut tcp_stream = TcpStream::connect(fixture.server_addr).await.unwrap();

    // Send PROXY protocol v2 header
    tcp_stream.write_all(&proxy_header).await.unwrap();

    // TLS handshake
    let tls_config = fixture.build_tls_client_config();
    let connector = TlsConnector::from(tls_config);
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls_stream = connector.connect(server_name, tcp_stream).await.unwrap();

    // Build HTTP/2 channel
    let (h2_sender, h2_conn) = h2::client::handshake(tls_stream).await.unwrap();
    tokio::spawn(async move {
        if let Err(e) = h2_conn.await {
            eprintln!("HTTP/2 connection error: {}", e);
        }
    });

    let mut sender = h2_sender.ready().await.unwrap();

    let request = http::Request::builder()
        .method("POST")
        .uri(format!(
            "https://{}/kuiper_agent_proto.RegistrationService/Register",
            fixture.server_addr
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .unwrap();

    let (response, _send_stream) = sender.send_request(request, false).unwrap();
    let response = response.await.unwrap();

    // Connection should succeed through PROXY protocol v2
    assert!(
        response.status().is_success() || response.status() == http::StatusCode::OK,
        "Expected HTTP 200, got {}",
        response.status()
    );
}

/// Test that server with proxy_protocol=true rejects connections without PROXY header
#[tokio::test]
async fn test_proxy_protocol_required_when_enabled() {
    install_crypto_provider();

    // Start server with proxy protocol enabled
    let fixture = TestFixture::new_with_proxy_protocol(false, true).await;

    // Try to connect WITHOUT sending PROXY protocol header
    let tcp_stream = TcpStream::connect(fixture.server_addr).await.unwrap();

    // Attempt TLS handshake directly (should fail because server expects PROXY header first)
    let tls_config = fixture.build_tls_client_config();
    let connector = TlsConnector::from(tls_config);
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();

    // The TLS handshake should fail because the server is waiting for PROXY header
    // and the TLS ClientHello will be interpreted as invalid PROXY protocol data
    let result = connector.connect(server_name, tcp_stream).await;
    assert!(
        result.is_err(),
        "Expected TLS handshake to fail when PROXY header not sent, but it succeeded"
    );
}

/// Test that webhook mode also works with PROXY protocol
#[tokio::test]
async fn test_webhook_mode_with_proxy_protocol_v1() {
    install_crypto_provider();

    // Start server with webhook mode AND proxy protocol
    let fixture = TestFixture::new_with_proxy_protocol(true, true).await;

    // Build PROXY protocol v1 header
    let proxy_header = build_proxy_v1_header("10.0.0.50", 54321, "10.0.0.1", 9443);

    let mut tcp_stream = TcpStream::connect(fixture.server_addr).await.unwrap();
    tcp_stream.write_all(&proxy_header).await.unwrap();
    tcp_stream.flush().await.unwrap();

    // TLS handshake
    let tls_config = fixture.build_tls_client_config();
    let connector = TlsConnector::from(tls_config);
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let tls_stream = connector.connect(server_name, tcp_stream).await.unwrap();

    // Build HTTP/2 channel
    let (h2_sender, h2_conn) = h2::client::handshake(tls_stream).await.unwrap();
    tokio::spawn(async move {
        if let Err(e) = h2_conn.await {
            eprintln!("HTTP/2 connection error: {}", e);
        }
    });

    let mut sender = h2_sender.ready().await.unwrap();

    let request = http::Request::builder()
        .method("POST")
        .uri(format!(
            "https://{}/kuiper_agent_proto.RegistrationService/Register",
            fixture.server_addr
        ))
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .unwrap();

    let (response, _send_stream) = sender.send_request(request, false).unwrap();
    let response = response.await.unwrap();

    assert!(
        response.status().is_success() || response.status() == http::StatusCode::OK,
        "Expected HTTP 200 in webhook mode with PROXY protocol, got {}",
        response.status()
    );
}
