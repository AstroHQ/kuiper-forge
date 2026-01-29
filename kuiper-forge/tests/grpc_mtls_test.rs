//! Integration tests for gRPC and mTLS functionality.
//!
//! These tests verify that:
//! - gRPC works over TLS (ALPN h2 negotiation)
//! - Registration service works without client cert
//! - Agent service requires mTLS (rejects requests without client cert)
//! - Both gRPC-only and webhook modes work correctly

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kuiper_agent_proto::{
    AgentMessage, AgentPayload, AgentServiceClient, AgentStatus, RegisterRequest,
    RegistrationServiceClient,
};
use tempfile::TempDir;
use tokio::sync::oneshot;
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
        use kuiper_forge::auth::{AuthManager, AuthStore, generate_server_cert, init_ca};
        use kuiper_forge::config::{DatabaseConfig, TlsConfig, WebhookConfig};
        use kuiper_forge::tls::build_server_trust;
        use kuiper_forge::db::Database;

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
        };

        // Create server config
        let server_config = kuiper_forge::server::ServerConfig {
            listen_addr: server_addr,
            tls: tls_config,
        };

        // Create webhook config and pending job store if needed
        let (webhook_config, pending_job_store) = if webhook_mode {
            let (tx, _rx) = tokio::sync::mpsc::channel(32);
            let webhook_notifier = kuiper_forge::webhook::WebhookNotifier::new(tx);
            let wh_config = WebhookConfig {
                path: "/webhook".to_string(),
                secret: "test-secret".to_string(),
                label_mappings: vec![],
            };
            let pending_store =
                Arc::new(kuiper_forge::pending_jobs::PendingJobStore::new(db.pool()));
            (Some((wh_config, webhook_notifier)), Some(pending_store))
        } else {
            (None, None)
        };

        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        let server_trust = build_server_trust(&tls_config).unwrap();

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
