use crate::error::LockError;
use crate::record::LeaderElectionRecord;

use k8s_openapi::api::coordination::v1::{Lease, LeaseSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
use k8s_openapi::jiff::Timestamp;
use kube::api::{Api, ObjectMeta, PostParams};
use tokio::sync::Mutex;

/// Trait representing a distributed lock backed by a Kubernetes resource.
///
/// Implementations manage the lifecycle of a Lease (or similar) object used
/// for leader election. The raw bytes returned by `get` enable byte-level
/// comparison to detect external mutations.
// ResourceLock is used exclusively as a generic bound (L: ResourceLock),
// never as dyn ResourceLock. Object safety is not required.
#[allow(async_fn_in_trait)]
pub trait ResourceLock: Send + Sync {
    /// Retrieve the current leader election record and its raw serialized form.
    async fn get(&self) -> Result<(LeaderElectionRecord, Vec<u8>), LockError>;

    /// Create a new leader election record. Fails if one already exists.
    async fn create(&self, record: &LeaderElectionRecord) -> Result<(), LockError>;

    /// Update an existing leader election record. Fails on version conflict.
    async fn update(&self, record: &LeaderElectionRecord) -> Result<(), LockError>;

    /// Return the identity string of this lock holder.
    fn identity(&self) -> &str;

    /// Return a human-readable description of the lock (e.g. namespace/name).
    fn describe(&self) -> String;

    /// Record a Kubernetes Event describing a state transition.
    /// Default is a no-op; implementations may override to emit events.
    fn record_event(&self, _message: &str) {}
}

/// A distributed lock backed by a Kubernetes `Lease` resource.
///
/// Uses `Api::replace()` (HTTP PUT with `resourceVersion`) for atomic
/// compare-and-swap updates.
pub struct LeaseLock {
    lease_api: Api<Lease>,
    name: String,
    namespace: String,
    identity: String,
    /// Cached lease for `resourceVersion` tracking between get/update cycles.
    cached_lease: Mutex<Option<Lease>>,
}

impl LeaseLock {
    /// Create a new `LeaseLock` targeting the given Lease resource name.
    ///
    /// The namespace is read from the in-cluster service account and the
    /// holder identity is auto-generated as `{hostname}_{uuid}`.
    ///
    /// Returns an error if the namespace or hostname cannot be determined
    /// (e.g., when running outside a Kubernetes cluster).
    pub fn new(client: kube::Client, name: String) -> Result<Self, std::io::Error> {
        let namespace = in_cluster_namespace()?;
        let identity = generate_identity()?;
        Ok(Self::with_options(client, name, namespace, identity))
    }

    /// Create a `LeaseLock` with explicit namespace and identity.
    pub(crate) fn with_options(
        client: kube::Client,
        name: String,
        namespace: String,
        identity: String,
    ) -> Self {
        let lease_api = Api::namespaced(client, &namespace);
        Self {
            lease_api,
            name,
            namespace,
            identity,
            cached_lease: Mutex::new(None),
        }
    }
}

impl ResourceLock for LeaseLock {
    async fn get(&self) -> Result<(LeaderElectionRecord, Vec<u8>), LockError> {
        let lease = self
            .lease_api
            .get(&self.name)
            .await
            .map_err(kube_error_to_lock_error)?;

        let record = lease_spec_to_record(lease.spec.as_ref());
        let raw = serde_json::to_vec(&record)?;

        *self.cached_lease.lock().await = Some(lease);

        Ok((record, raw))
    }

    async fn create(&self, record: &LeaderElectionRecord) -> Result<(), LockError> {
        let lease = Lease {
            metadata: ObjectMeta {
                name: Some(self.name.clone()),
                namespace: Some(self.namespace.clone()),
                ..ObjectMeta::default()
            },
            spec: Some(record_to_lease_spec(record)?),
        };

        let created = self
            .lease_api
            .create(&PostParams::default(), &lease)
            .await
            .map_err(kube_error_to_lock_error)?;

        *self.cached_lease.lock().await = Some(created);

        Ok(())
    }

    async fn update(&self, record: &LeaderElectionRecord) -> Result<(), LockError> {
        let mut lease = {
            let cached = self.cached_lease.lock().await;
            cached.clone().ok_or(LockError::NotInitialized)?
        };

        lease.spec = Some(record_to_lease_spec(record)?);

        let updated = self
            .lease_api
            .replace(&self.name, &PostParams::default(), &lease)
            .await
            .map_err(kube_error_to_lock_error)?;

        *self.cached_lease.lock().await = Some(updated);

        Ok(())
    }

    fn identity(&self) -> &str {
        &self.identity
    }

    fn describe(&self) -> String {
        format!("{}/{}", self.namespace, self.name)
    }
}

/// Convert a `LeaderElectionRecord` into a Kubernetes `LeaseSpec`.
fn record_to_lease_spec(record: &LeaderElectionRecord) -> Result<LeaseSpec, LockError> {
    let acquire_time = parse_micro_time(&record.acquire_time)?;
    let renew_time = parse_micro_time(&record.renew_time)?;

    Ok(LeaseSpec {
        holder_identity: Some(record.holder_identity.clone()),
        lease_duration_seconds: Some(record.lease_duration_seconds),
        acquire_time,
        renew_time,
        lease_transitions: Some(record.leader_transitions),
        preferred_holder: if record.preferred_holder.is_empty() {
            None
        } else {
            Some(record.preferred_holder.clone())
        },
        strategy: if record.strategy.is_empty() {
            None
        } else {
            Some(record.strategy.clone())
        },
    })
}

/// Extract a `LeaderElectionRecord` from an optional Kubernetes `LeaseSpec`.
fn lease_spec_to_record(spec: Option<&LeaseSpec>) -> LeaderElectionRecord {
    let Some(spec) = spec else {
        return LeaderElectionRecord::default();
    };

    LeaderElectionRecord {
        holder_identity: spec.holder_identity.clone().unwrap_or_default(),
        lease_duration_seconds: spec.lease_duration_seconds.unwrap_or_default(),
        acquire_time: spec
            .acquire_time
            .as_ref()
            .map(|t| format!("{:.6}", t.0))
            .unwrap_or_default(),
        renew_time: spec
            .renew_time
            .as_ref()
            .map(|t| format!("{:.6}", t.0))
            .unwrap_or_default(),
        leader_transitions: spec.lease_transitions.unwrap_or_default(),
        preferred_holder: spec.preferred_holder.clone().unwrap_or_default(),
        strategy: spec.strategy.clone().unwrap_or_default(),
    }
}

/// Parse an RFC 3339 timestamp string into a `MicroTime`.
///
/// Returns `Ok(None)` for empty strings, `Err` for malformed timestamps.
fn parse_micro_time(s: &str) -> Result<Option<MicroTime>, LockError> {
    if s.is_empty() {
        return Ok(None);
    }
    s.parse::<Timestamp>()
        .map(|ts| Some(MicroTime(ts)))
        .map_err(|e| LockError::InvalidTimestamp(s.to_owned(), e.to_string()))
}

const IN_CLUSTER_NAMESPACE_PATH: &str = "/var/run/secrets/kubernetes.io/serviceaccount/namespace";

/// Read the namespace from the in-cluster service account.
///
/// Returns an `io::Error` if the file does not exist or cannot be read
/// (e.g., when running outside a Kubernetes cluster).
pub(crate) fn in_cluster_namespace() -> Result<String, std::io::Error> {
    let ns = std::fs::read_to_string(IN_CLUSTER_NAMESPACE_PATH)?;
    Ok(ns.trim().to_owned())
}

/// Generate a unique holder identity as `{hostname}_{uuid}`.
///
/// Returns an `io::Error` if the system hostname cannot be determined.
pub(crate) fn generate_identity() -> Result<String, std::io::Error> {
    let hostname = hostname()?;
    Ok(format!("{}_{}", hostname, uuid::Uuid::new_v4()))
}

fn hostname() -> Result<String, std::io::Error> {
    let mut buf = [0u8; 256];
    // SAFETY: `buf` is a valid, mutable, 256-byte buffer. `gethostname` writes
    // at most `buf.len()` bytes including a null terminator (or truncates).
    // The pointer cast from `*mut u8` to `*mut c_char` is valid because
    // `c_char` is a byte-sized type with the same alignment.
    let ret = unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8(buf[..len].to_vec())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn kube_error_to_lock_error(err: kube::Error) -> LockError {
    match &err {
        kube::Error::Api(ae) if ae.code == 404 => LockError::NotFound,
        kube::Error::Api(ae) if ae.code == 409 => LockError::Conflict,
        _ => LockError::Kube(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_record_through_lease_spec() {
        let record = LeaderElectionRecord {
            holder_identity: "node-1".to_owned(),
            lease_duration_seconds: 15,
            acquire_time: "2025-01-15T10:30:00.000000Z".to_owned(),
            renew_time: "2025-01-15T10:30:05.000000Z".to_owned(),
            leader_transitions: 3,
            preferred_holder: String::new(),
            strategy: String::new(),
        };

        let spec = record_to_lease_spec(&record).expect("conversion to lease spec should succeed");
        let back = lease_spec_to_record(Some(&spec));

        assert_eq!(record.holder_identity, back.holder_identity);
        assert_eq!(record.lease_duration_seconds, back.lease_duration_seconds);
        assert_eq!(record.leader_transitions, back.leader_transitions);
        // Compare parsed timestamps to handle any formatting differences.
        let orig_acquire: Timestamp = record.acquire_time.parse().unwrap();
        let back_acquire: Timestamp = back.acquire_time.parse().unwrap();
        assert_eq!(orig_acquire, back_acquire, "acquire_time must round-trip");

        let orig_renew: Timestamp = record.renew_time.parse().unwrap();
        let back_renew: Timestamp = back.renew_time.parse().unwrap();
        assert_eq!(orig_renew, back_renew, "renew_time must round-trip");
    }

    #[test]
    fn lease_spec_to_record_with_none_returns_default() {
        let record = lease_spec_to_record(None);
        assert_eq!(record, LeaderElectionRecord::default());
    }

    #[test]
    fn parse_micro_time_empty_returns_none() {
        let result = parse_micro_time("").expect("empty string should not error");
        assert!(result.is_none());
    }

    #[test]
    fn parse_micro_time_invalid_returns_error() {
        let result = parse_micro_time("bad-timestamp");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, LockError::InvalidTimestamp(_, _)));
    }

    #[test]
    fn parse_micro_time_valid_timestamp() {
        let result =
            parse_micro_time("2025-01-15T10:30:00.000000Z").expect("valid timestamp should parse");
        assert!(result.is_some());
    }

    #[test]
    fn record_with_preferred_holder_round_trips() {
        let record = LeaderElectionRecord {
            holder_identity: "node-1".to_owned(),
            lease_duration_seconds: 15,
            acquire_time: "2025-01-15T10:30:00.000000Z".to_owned(),
            renew_time: "2025-01-15T10:30:05.000000Z".to_owned(),
            leader_transitions: 0,
            preferred_holder: "node-2".to_owned(),
            strategy: "OldestEmulationVersion".to_owned(),
        };

        let spec = record_to_lease_spec(&record).expect("conversion should succeed");
        assert_eq!(spec.preferred_holder, Some("node-2".to_owned()));
        assert_eq!(spec.strategy, Some("OldestEmulationVersion".to_owned()));

        let back = lease_spec_to_record(Some(&spec));
        assert_eq!(record.preferred_holder, back.preferred_holder);
        assert_eq!(record.strategy, back.strategy);
    }
}
