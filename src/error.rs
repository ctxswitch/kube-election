use thiserror::Error;

/// Errors related to leader election configuration and lifecycle.
#[derive(Debug, Error)]
pub enum LeaderElectionError {
    /// The configuration is invalid.
    #[error("invalid config: {0}")]
    InvalidConfig(&'static str),
}

/// Errors returned by resource lock operations.
#[derive(Debug, Error)]
pub enum LockError {
    /// The backing resource (Lease) was not found.
    #[error("resource not found")]
    NotFound,
    /// The resource version did not match, indicating a concurrent update.
    #[error("conflict: resource version mismatch")]
    Conflict,
    /// The lock was not initialized (get or create must be called before update).
    #[error("lock not initialized: call get or create before update")]
    NotInitialized,
    /// An error from the Kubernetes API client.
    #[error("kubernetes API error: {0}")]
    Kube(#[from] kube::Error),
    /// A serialization or deserialization error.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    /// A timestamp string could not be parsed.
    #[error("invalid timestamp {0:?}: {1}")]
    InvalidTimestamp(String, String),
}
