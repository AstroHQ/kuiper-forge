//! The Rust CLI's management client (`kuiper-forge/src/management.rs` ManagementClient, used by
//! `kuiper-forge token` and `kuiper-forge agent`) against the Phoenix management socket.
//!
//! usage: kuiper-management-probe <socket> <agent id to revoke>
//!
//! Creates a token, lists and deletes it, lists agents and revokes the given one, the way the
//! CLI does. Prints `PASS <step>` per step and `ALL PASS` at the end; panics on any failure.
use hyper_util::rt::TokioIo;
use kuiper_agent_proto::ManagementServiceClient;
use kuiper_agent_proto::management::{
    CreateTokenRequest, DeleteTokenRequest, ListAgentsRequest, ListTokensRequest,
    RevokeAgentRequest,
};
use tokio::net::UnixStream;
use tonic::transport::{Endpoint, Uri};
use tower::service_fn;

fn pass(step: &str) {
    println!("PASS {step}");
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (socket, agent_id) = (args[1].clone(), args[2].clone());

    // same connector as ManagementClient::connect
    let channel = Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(service_fn(move |_: Uri| {
            let path = socket.clone();
            async move {
                let stream = UnixStream::connect(path).await?;
                Ok::<_, std::io::Error>(TokioIo::new(stream))
            }
        }))
        .await
        .expect("connects to the management socket");
    let mut client = ManagementServiceClient::new(channel);
    pass("connect");

    let created = client
        .create_token(CreateTokenRequest {
            expires_secs: 3600,
            created_by: "cli".into(),
        })
        .await
        .expect("CreateToken")
        .into_inner();
    assert!(created.token.starts_with("reg_"), "{created:?}");
    assert_eq!(created.server_trust_mode, "ca");
    assert!(created.server_ca_pem.contains("BEGIN CERTIFICATE"));
    chrono_like(&created.expires_at);
    pass("create_token");
    println!("TOKEN {}", created.token);

    let bad = client
        .create_token(CreateTokenRequest {
            expires_secs: 0,
            created_by: "cli".into(),
        })
        .await
        .expect_err("a zero TTL is refused");
    assert_eq!(bad.code(), tonic::Code::InvalidArgument);
    pass("create_token_zero_ttl_refused");

    let tokens = client
        .list_tokens(ListTokensRequest {})
        .await
        .expect("ListTokens")
        .into_inner()
        .tokens;
    let t = tokens
        .iter()
        .find(|t| t.token == created.token)
        .expect("listed");
    assert_eq!(t.created_by, "cli");
    pass("list_tokens");

    let del = client
        .delete_token(DeleteTokenRequest {
            token: created.token.clone(),
        })
        .await
        .expect("DeleteToken")
        .into_inner();
    assert!(del.deleted);
    pass("delete_token");

    let agents = client
        .list_agents(ListAgentsRequest {})
        .await
        .expect("ListAgents")
        .into_inner()
        .agents;
    let a = agents
        .iter()
        .find(|a| a.agent_id == agent_id)
        .expect("agent listed");
    assert!(!a.revoked);
    assert_eq!(a.hostname, "probe-host");
    chrono_like(&a.created_at);
    pass("list_agents");

    let rev = client
        .revoke_agent(RevokeAgentRequest {
            agent_id: agent_id.clone(),
        })
        .await
        .expect("RevokeAgent")
        .into_inner();
    assert!(rev.revoked);
    pass("revoke_agent");

    println!("ALL PASS");
}

// RFC 3339 as the CLI prints it: date, 'T', time, zone
fn chrono_like(s: &str) {
    assert!(
        s.len() >= 20 && s.as_bytes()[10] == b'T' && (s.ends_with('Z') || s.contains('+')),
        "not RFC 3339: {s}"
    );
}
