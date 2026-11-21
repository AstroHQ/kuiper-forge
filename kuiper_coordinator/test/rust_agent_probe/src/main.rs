//! A real kuiper-agent-lib agent driven against the Phoenix coordinator, the way
//! kuiper-tart-agent registers (setup.rs) and runs (runtime::connect).
//!
//! usage: kuiper-agent-probe <registration bundle> <cert dir>
//!
//! 1. AgentStream and UploadLogs without a client cert must get Unauthenticated.
//! 2. Decode the bundle, save its CA and trust mode, register with its token.
//! 3. Connect with the issued cert and stream status; wait for a CreateRunner,
//!    ack it and report the runner Started.
//! 4. UploadLogs over mTLS.
//! Prints `PASS <step>` per step and `ALL PASS` at the end; panics on any failure.
use std::time::Duration;

use kuiper_agent_lib::bundle::{RegistrationBundle, ServerTrustMode};
use kuiper_agent_lib::runtime::{self, RunnerCommand};
use kuiper_agent_lib::{AgentCertStore, AgentConfig, AgentConnector};
use kuiper_agent_proto::{AgentMessage, AgentServiceClient, AgentStatus, LogBatch, LogRecord};
use tokio::sync::watch;
use tonic::transport::{Certificate, Channel, ClientTlsConfig};

fn pass(step: &str) {
    println!("PASS {step}");
}

#[tokio::main]
async fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".into()))
        .init();
    let args: Vec<String> = std::env::args().collect();
    let (bundle, dir) = (&args[1], &args[2]);

    let bundle = RegistrationBundle::decode(bundle).expect("bundle decodes");
    assert_eq!(bundle.server_trust_mode, ServerTrustMode::Ca);
    let ca = bundle.server_ca_pem.clone().expect("bundle carries the CA");
    let url = bundle.coordinator_url.clone();
    let host = url::Url::parse(&url)
        .unwrap()
        .host_str()
        .unwrap()
        .to_string();
    pass("bundle decodes with kuiper-agent-lib");

    // 1. server-only TLS works, but AgentService needs a client cert
    let tls = ClientTlsConfig::new()
        .ca_certificate(Certificate::from_pem(&ca))
        .domain_name(host.clone());
    let ch: Channel = Channel::from_shared(url.clone())
        .unwrap()
        .tls_config(tls)
        .unwrap()
        .connect()
        .await
        .expect("server-TLS connect");
    let mut anon = AgentServiceClient::new(ch);
    let (_tx, rx) = tokio::sync::mpsc::channel::<AgentMessage>(1);
    match anon
        .agent_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
    {
        Err(s) if s.code() == tonic::Code::Unauthenticated => {
            pass("AgentStream without client cert -> Unauthenticated")
        }
        other => panic!("expected Unauthenticated, got {other:?}"),
    }
    match anon.upload_logs(LogBatch::default()).await {
        Err(s) if s.code() == tonic::Code::Unauthenticated => {
            pass("UploadLogs without client cert -> Unauthenticated")
        }
        other => panic!("expected Unauthenticated, got {other:?}"),
    }

    // 2. register like kuiper-tart-agent setup.rs
    let store = AgentCertStore::new(dir.into());
    store.save_ca(&ca).unwrap();
    store.save_server_trust_mode("ca").unwrap();
    let config = AgentConfig {
        coordinator_url: url.clone(),
        coordinator_hostname: host.clone(),
        registration_token: Some(bundle.token.clone()),
        agent_type: "tart".into(),
    };
    let mut connector = AgentConnector::new(config.clone(), store.clone());
    connector.register().await.expect("register");
    let agent_id = store.get_agent_id().expect("agent id saved");
    pass(&format!("registered as {agent_id}"));

    // 3. the agent runtime: mTLS stream, first status, commands
    let (_status_tx, status_rx) = watch::channel(AgentStatus {
        hostname: "probe-host".into(),
        agent_type: "tart".into(),
        labels: vec!["self-hosted".into(), "macOS".into()],
        max_vms: 2,
        available_slots: 2,
        agent_version: "0.0.0-probe".into(),
        ..Default::default()
    });
    let cfg = AgentConfig {
        registration_token: None,
        ..config.clone()
    };
    let mut conn = runtime::connect(
        cfg.clone(),
        AgentCertStore::new(dir.into()),
        status_rx,
        Duration::from_millis(200),
        Duration::from_secs(2),
    );
    let cmd = tokio::time::timeout(Duration::from_secs(30), conn.commands.recv())
        .await
        .expect("no command in 30s")
        .expect("stream closed");
    let RunnerCommand::Create(c) = cmd else {
        panic!("expected CreateRunner, got {cmd:?}")
    };
    pass(&format!(
        "got CreateRunner command_id={} vm_name={}",
        c.command_id, c.vm_name
    ));
    conn.events
        .command_ack(c.command_id.clone(), true, String::new())
        .await
        .unwrap();
    conn.events
        .runner_event(
            c.vm_name.clone(),
            "vm-1".into(),
            kuiper_agent_proto::RunnerEventType::Started,
            String::new(),
            None,
        )
        .await
        .unwrap();

    // 4. UploadLogs over mTLS with the stored cert
    let mut c2 = AgentConnector::new(cfg, AgentCertStore::new(dir.into()));
    let mut client = c2.connect().await.expect("mTLS connect with stored cert");
    let batch = LogBatch {
        records: vec![LogRecord {
            timestamp_micros: 1_700_000_000_000_000,
            level: "INFO".into(),
            target: "probe".into(),
            message: "hello from the probe".into(),
            seq: 1,
        }],
        dropped: 0,
    };
    client.upload_logs(batch).await.expect("upload_logs");
    pass("UploadLogs with client cert");

    // give the runtime a moment to flush the ack and runner event
    tokio::time::sleep(Duration::from_secs(1)).await;
    println!("ALL PASS");
}
