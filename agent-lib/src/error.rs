use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("gRPC transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    #[error("gRPC status error: {0}")]
    Status(#[from] tonic::Status),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("No credentials available: {0}")]
    NoCredentials(String),

    #[error("Authentication failed: {0}")]
    AuthFailed(String),

    #[error("Certificate error: {0}")]
    Certificate(String),

    #[error("Registration failed: {0}")]
    RegistrationFailed(String),

    #[error("Connection lost")]
    ConnectionLost,
}

impl Error {
    /// Returns true if this error indicates an authentication problem
    /// that might be resolved by re-registering with a new token.
    pub fn is_auth_error(&self) -> bool {
        match self {
            Error::AuthFailed(_) => true,
            Error::Status(status) => {
                matches!(
                    status.code(),
                    tonic::Code::Unauthenticated | tonic::Code::PermissionDenied
                )
            }
            _ => false,
        }
    }
}
