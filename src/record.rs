use serde::{Deserialize, Serialize};

/// Record stored in the Lease object representing the current leader election state.
///
/// Maps 1:1 to the Lease spec fields. Derives `Serialize`/`Deserialize` for raw
/// byte comparison, matching Go's `observedRawRecord` pattern.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LeaderElectionRecord {
    /// The identity of the current holder of the lease.
    pub holder_identity: String,
    /// Duration of the lease in seconds.
    pub lease_duration_seconds: i32,
    /// RFC 3339 microsecond timestamp of when the lease was acquired.
    pub acquire_time: String,
    /// RFC 3339 microsecond timestamp of when the lease was last renewed.
    pub renew_time: String,
    /// Number of times the lease has transitioned between holders.
    pub leader_transitions: i32,
    /// Used in coordinated leader election to signal voluntary step-down.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub preferred_holder: String,
    /// Coordinated lease strategy. Empty in standard mode.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub strategy: String,
}
