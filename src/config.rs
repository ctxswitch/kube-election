use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::error::LeaderElectionError;
use crate::lock::ResourceLock;

/// Callback invoked when this instance becomes the leader.
type OnStartedLeading =
    Box<dyn Fn(CancellationToken) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Callback invoked when a new leader is observed.
/// Wrapped in `Arc` so it can be spawned into a separate task,
/// matching Go's goroutine dispatch in `maybeReportTransition`.
type OnNewLeader = Arc<dyn Fn(&str) + Send + Sync>;

/// Jitter factor applied when comparing `renew_deadline` against `retry_period`.
///
/// Matches the Go constant `JitterFactor = 1.2`.
pub const JITTER_FACTOR: f64 = 1.2;

/// Configuration for a leader election instance.
///
/// Generic over the resource lock implementation.
pub struct LeaderElectionConfig<L: ResourceLock> {
    /// The distributed lock backing this election.
    pub lock: L,
    /// How long a non-leader candidate waits before attempting to acquire the
    /// lease after observing it.
    pub lease_duration: Duration,
    /// How long the acting leader retries refreshing before giving up.
    pub renew_deadline: Duration,
    /// Interval between attempts to acquire or renew the lease.
    pub retry_period: Duration,
    /// Whether to release the lease when the cancellation token fires.
    pub release_on_cancel: bool,
    /// Human-readable name for this election (used in logging).
    pub name: String,
    /// Callbacks invoked on leadership transitions.
    pub callbacks: LeaderCallbacks,
}

/// Callbacks invoked during leader election lifecycle events.
pub struct LeaderCallbacks {
    /// Called when this instance becomes the leader. Receives a
    /// `CancellationToken` that is cancelled when leadership is lost.
    pub on_started_leading: OnStartedLeading,
    /// Called when this instance stops leading (either voluntarily or due to
    /// failure to renew). Always called when the elector exits, even if it
    /// never acquired leadership.
    pub on_stopped_leading: Box<dyn FnOnce() + Send + Sync>,
    /// Called when a new leader is observed. Receives the identity of the new
    /// leader. `None` if not needed.
    pub on_new_leader: Option<OnNewLeader>,
}

impl<L: ResourceLock> LeaderElectionConfig<L> {
    /// Validate the configuration, returning an error if any invariants are
    /// violated.
    ///
    /// Mirrors the validation in Go's `NewLeaderElector`.
    pub(crate) fn validate(&self) -> Result<(), LeaderElectionError> {
        if self.lease_duration.is_zero() {
            return Err(LeaderElectionError::InvalidConfig(
                "lease_duration must be greater than zero",
            ));
        }

        if self.renew_deadline.is_zero() {
            return Err(LeaderElectionError::InvalidConfig(
                "renew_deadline must be greater than zero",
            ));
        }

        if self.retry_period.is_zero() {
            return Err(LeaderElectionError::InvalidConfig(
                "retry_period must be greater than zero",
            ));
        }

        if self.lease_duration <= self.renew_deadline {
            return Err(LeaderElectionError::InvalidConfig(
                "lease_duration must be greater than renew_deadline",
            ));
        }

        let jittered_retry =
            Duration::from_secs_f64(self.retry_period.as_secs_f64() * JITTER_FACTOR);
        if self.renew_deadline <= jittered_retry {
            return Err(LeaderElectionError::InvalidConfig(
                "renew_deadline must be greater than retry_period * JITTER_FACTOR",
            ));
        }

        if self.lock.identity().is_empty() {
            return Err(LeaderElectionError::InvalidConfig(
                "lock identity must not be empty",
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal mock lock for config validation tests.
    struct StubLock {
        identity: String,
    }

    impl StubLock {
        fn new(identity: &str) -> Self {
            Self {
                identity: identity.to_owned(),
            }
        }
    }

    impl ResourceLock for StubLock {
        async fn get(
            &self,
        ) -> Result<(crate::record::LeaderElectionRecord, Vec<u8>), crate::error::LockError>
        {
            unimplemented!()
        }
        async fn create(
            &self,
            _record: &crate::record::LeaderElectionRecord,
        ) -> Result<(), crate::error::LockError> {
            unimplemented!()
        }
        async fn update(
            &self,
            _record: &crate::record::LeaderElectionRecord,
        ) -> Result<(), crate::error::LockError> {
            unimplemented!()
        }
        fn identity(&self) -> &str {
            &self.identity
        }
        fn describe(&self) -> String {
            "stub".to_owned()
        }
    }

    fn make_config(
        lock: StubLock,
        lease: Duration,
        renew: Duration,
        retry: Duration,
    ) -> LeaderElectionConfig<StubLock> {
        LeaderElectionConfig {
            lock,
            lease_duration: lease,
            renew_deadline: renew,
            retry_period: retry,
            release_on_cancel: false,
            name: "test".to_owned(),
            callbacks: LeaderCallbacks {
                on_started_leading: Box::new(|token| {
                    Box::pin(async move {
                        token.cancelled().await;
                    })
                }),
                on_stopped_leading: Box::new(|| {}),
                on_new_leader: None,
            },
        }
    }

    #[test]
    fn valid_config_passes_validation() {
        let cfg = make_config(
            StubLock::new("node-1"),
            Duration::from_secs(15),
            Duration::from_secs(10),
            Duration::from_secs(2),
        );
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn lease_duration_must_exceed_renew_deadline() {
        // lease_duration == renew_deadline should fail
        let cfg = make_config(
            StubLock::new("node-1"),
            Duration::from_secs(10),
            Duration::from_secs(10),
            Duration::from_secs(2),
        );
        let err = cfg.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("lease_duration must be greater than renew_deadline"));

        // lease_duration < renew_deadline should fail
        let cfg = make_config(
            StubLock::new("node-1"),
            Duration::from_secs(5),
            Duration::from_secs(10),
            Duration::from_secs(2),
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn renew_deadline_must_exceed_jittered_retry_period() {
        // renew_deadline = retry_period * JITTER_FACTOR exactly should fail (<=)
        // retry=5, jittered=6.0, renew=6 => renew <= jittered => fail
        let cfg = make_config(
            StubLock::new("node-1"),
            Duration::from_secs(15),
            Duration::from_secs(6),
            Duration::from_secs(5),
        );
        let err = cfg.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("renew_deadline must be greater than retry_period * JITTER_FACTOR"));
    }

    #[test]
    fn all_zero_durations_rejected() {
        let cfg = make_config(
            StubLock::new("node-1"),
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        let err = cfg.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("lease_duration must be greater than zero"));
    }

    #[test]
    fn zero_retry_period_rejected() {
        // lease > renew > 0, but retry = 0 => jittered = 0, renew > 0 passes jitter check,
        // but retry_period.is_zero() should catch it.
        let cfg = make_config(
            StubLock::new("node-1"),
            Duration::from_secs(15),
            Duration::from_secs(10),
            Duration::ZERO,
        );
        let err = cfg.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("retry_period must be greater than zero"));
    }

    #[test]
    fn empty_identity_rejected() {
        let cfg = make_config(
            StubLock::new(""),
            Duration::from_secs(15),
            Duration::from_secs(10),
            Duration::from_secs(2),
        );
        let err = cfg.validate().unwrap_err();
        assert!(err.to_string().contains("lock identity must not be empty"));
    }
}
