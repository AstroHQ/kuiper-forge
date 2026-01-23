/// Generated gRPC code from proto/agent.proto
pub mod proto {
    tonic::include_proto!("cirunner.agent");
}

/// Generated gRPC code from proto/management.proto
pub mod management {
    tonic::include_proto!("cirunner.management");
}

// Re-export commonly used types at the crate root for convenience
pub use proto::agent_service_client::AgentServiceClient;
pub use proto::agent_service_server::{AgentService, AgentServiceServer};
pub use proto::registration_service_client::RegistrationServiceClient;
pub use proto::registration_service_server::{RegistrationService, RegistrationServiceServer};

pub use proto::{
    AgentMessage, AgentStatus, CommandAck, CommandResult, CoordinatorMessage, CreateRunnerCommand,
    CreateRunnerResult, DestroyRunnerCommand, DestroyRunnerResult, Ping, Pong, RegisterRequest,
    RegisterResponse, RunnerEvent, VmInfo,
};

// Re-export the payload enums for pattern matching
pub use proto::agent_message::Payload as AgentPayload;
pub use proto::command_result::Result as CommandResultPayload;
pub use proto::coordinator_message::Payload as CoordinatorPayload;
pub use proto::runner_event::RunnerEventType;

// Re-export management service types
pub use management::management_service_client::ManagementServiceClient;
pub use management::management_service_server::{ManagementService, ManagementServiceServer};
pub use management::{
    AgentInfo, CreateTokenRequest, CreateTokenResponse, DeleteTokenRequest, DeleteTokenResponse,
    ListAgentsRequest, ListAgentsResponse, ListTokensRequest, ListTokensResponse, RevokeAgentRequest,
    RevokeAgentResponse, TokenInfo,
};
