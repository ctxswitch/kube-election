use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use tokio::sync::{Mutex, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::{LeaderElectionConfig, JITTER_FACTOR};
use crate::error::{LeaderElectionError, LockError};
use crate::lock::ResourceLock;
use crate::record::LeaderElectionRecord;

/// Consolidated observed state, protected by a single `RwLock` to ensure
/// atomicity across record, raw bytes, and observed time.
struct ObservedState {
    record: LeaderElectionRecord,
    raw: Vec<u8>,
    time: Instant,
}

/// Leader election client.
///
/// Uses local monotonic time (`tokio::time::Instant`) to determine lease
/// validity, making it tolerant to arbitrary clock skew between nodes.
pub struct LeaderElector<L: ResourceLock> {
    config: LeaderElectionConfig<L>,
    observed: RwLock<ObservedState>,
    reported_leader: Mutex<String>,
}

impl<L: ResourceLock + 'static> LeaderElector<L> {
    /// Create a new `LeaderElector` from the given configuration.
    ///
    /// Returns an error if the configuration fails validation.
    pub fn new(config: LeaderElectionConfig<L>) -> Result<Self, LeaderElectionError> {
        config.validate()?;
        Ok(Self {
            config,
            observed: RwLock::new(ObservedState {
                record: LeaderElectionRecord::default(),
                raw: Vec::new(),
                time: Instant::now(),
            }),
            reported_leader: Mutex::new(String::new()),
        })
    }

    /// Run the leader election loop.
    ///
    /// Always calls `on_stopped_leading` before returning, even if leadership
    /// was never acquired. Takes `self` by value so that `FnOnce` callbacks
    /// can be consumed.
    pub async fn run(self, token: CancellationToken) {
        let LeaderElector {
            mut config,
            observed,
            reported_leader,
        } = self;
        let on_stopped_leading =
            std::mem::replace(&mut config.callbacks.on_stopped_leading, Box::new(|| {}));

        let elector = LeaderElector {
            config,
            observed,
            reported_leader,
        };

        if !elector.acquire(&token).await {
            on_stopped_leading();
            return;
        }

        let child_token = token.child_token();
        let future = (elector.config.callbacks.on_started_leading)(child_token.clone());
        // Detached: on_started_leading runs independently, signalled via child_token.
        drop(tokio::spawn(future));

        elector.renew(&token).await;

        child_token.cancel();
        on_stopped_leading();
    }

    /// Attempt to acquire the leader lease, looping until success or
    /// cancellation.
    async fn acquire(&self, token: &CancellationToken) -> bool {
        let desc = self.config.lock.describe();
        tracing::info!(lock = %desc, "attempting to acquire leader lease");

        loop {
            let succeeded = self.try_acquire_or_renew().await;
            self.maybe_report_transition().await;

            if succeeded {
                self.config.lock.record_event("became leader");
                tracing::info!(lock = %desc, "successfully acquired lease");
                return true;
            }

            let sleep_dur = jittered_duration(self.config.retry_period);
            tokio::select! {
                () = token.cancelled() => return false,
                () = tokio::time::sleep(sleep_dur) => {}
            }
        }
    }

    /// Renew the leader lease until it is lost or the token is cancelled.
    async fn renew(&self, token: &CancellationToken) {
        let desc = self.config.lock.describe();

        loop {
            let deadline = tokio::time::sleep(self.config.renew_deadline);
            tokio::pin!(deadline);

            let success;
            loop {
                // Short-circuit if cancelled before making API calls.
                if token.is_cancelled() {
                    self.maybe_report_transition().await;
                    self.config.lock.record_event("stopped leading");
                    if self.config.release_on_cancel {
                        self.release().await;
                    }
                    return;
                }

                let renewed = self.try_acquire_or_renew().await;

                if renewed {
                    success = true;
                    break;
                }

                tokio::select! {
                    () = &mut deadline => {
                        success = false;
                        break;
                    }
                    () = token.cancelled() => {
                        self.maybe_report_transition().await;
                        self.config.lock.record_event("stopped leading");
                        if self.config.release_on_cancel {
                            self.release().await;
                        }
                        return;
                    }
                    () = tokio::time::sleep(self.config.retry_period) => {}
                }
            }

            self.maybe_report_transition().await;

            if !success {
                self.config.lock.record_event("stopped leading");
                tracing::info!(lock = %desc, "failed to renew lease");
                if self.config.release_on_cancel {
                    self.release().await;
                }
                return;
            }

            tracing::debug!(lock = %desc, "successfully renewed lease");

            tokio::select! {
                () = token.cancelled() => {
                    self.config.lock.record_event("stopped leading");
                    if self.config.release_on_cancel {
                        self.release().await;
                    }
                    return;
                }
                () = tokio::time::sleep(self.config.retry_period) => {}
            }
        }
    }

    /// Core leader election logic. Tries to acquire the lease if not held, or
    /// renew it if already held. Returns `true` on success.
    async fn try_acquire_or_renew(&self) -> bool {
        let now_ts = format!("{:.6}", k8s_openapi::jiff::Timestamp::now());
        let mut record = LeaderElectionRecord {
            holder_identity: self.config.lock.identity().to_owned(),
            lease_duration_seconds: i32::try_from(self.config.lease_duration.as_secs())
                .unwrap_or(i32::MAX),
            renew_time: now_ts.clone(),
            acquire_time: now_ts,
            ..LeaderElectionRecord::default()
        };

        let desc = self.config.lock.describe();

        // 1. Fast path: leader optimistically updates assuming observed record
        //    is current.
        if self.is_leader().await && self.is_lease_valid().await {
            let observed = self.get_observed_record().await;
            record.acquire_time = observed.acquire_time;
            record.leader_transitions = observed.leader_transitions;

            match self.config.lock.update(&record).await {
                Ok(()) => {
                    self.set_observed_record(&record).await;
                    return true;
                }
                Err(err) => {
                    tracing::error!(
                        lock = %desc,
                        %err,
                        "failed to update lease optimistically, falling back to slow path"
                    );
                }
            }
        }

        // 2. Obtain or create the election record.
        let (old_record, old_raw) = match self.config.lock.get().await {
            Ok(pair) => pair,
            Err(LockError::NotFound) => {
                if let Err(err) = self.config.lock.create(&record).await {
                    tracing::info!(
                        lock = %desc,
                        %err,
                        "error initially creating lease lock"
                    );
                    return false;
                }
                self.set_observed_record(&record).await;
                return true;
            }
            Err(err) => {
                tracing::info!(lock = %desc, %err, "error retrieving lease lock");
                return false;
            }
        };

        // 3. Record obtained, check identity and time.
        {
            let current_raw = self.observed.read().await;
            if current_raw.raw != old_raw {
                drop(current_raw);
                self.set_observed_record_with_raw(&old_record, old_raw)
                    .await;
            }
        }

        if !old_record.holder_identity.is_empty()
            && self.is_lease_valid().await
            && !self.is_leader().await
        {
            tracing::debug!(
                lock = %desc,
                holder = %old_record.holder_identity,
                "lease is held and has not yet expired"
            );
            return false;
        }

        // 4. Prepare the update.
        if self.is_leader().await {
            record.acquire_time = old_record.acquire_time;
            record.leader_transitions = old_record.leader_transitions;
        } else {
            record.leader_transitions = old_record.leader_transitions + 1;
        }

        if let Err(err) = self.config.lock.update(&record).await {
            tracing::info!(lock = %desc, %err, "failed to update lease");
            return false;
        }

        self.set_observed_record(&record).await;
        true
    }

    /// Check whether the observed lease is still valid based on local
    /// monotonic time.
    async fn is_lease_valid(&self) -> bool {
        let state = self.observed.read().await;
        let lease_dur = Duration::from_secs(state.record.lease_duration_seconds.max(0) as u64);
        state.time + lease_dur > Instant::now()
    }

    /// Release the leader lease by setting the holder to empty and the
    /// duration to 1 second. Guarded by a timeout of `renew_deadline`.
    async fn release(&self) {
        let timeout = self.config.renew_deadline;
        if tokio::time::timeout(timeout, self.release_inner())
            .await
            .is_err()
        {
            tracing::warn!(lock = %self.config.lock.describe(), "timed out releasing lease");
        }
    }

    async fn release_inner(&self) {
        let desc = self.config.lock.describe();

        let old_record = match self.config.lock.get().await {
            Ok((rec, _raw)) => rec,
            Err(LockError::NotFound) => {
                tracing::info!(lock = %desc, "lease lock not found during release");
                return;
            }
            Err(err) => {
                tracing::info!(lock = %desc, %err, "error retrieving resource lock during release");
                return;
            }
        };

        if !self.is_leader().await {
            return;
        }

        let now_ts = format!("{:.6}", k8s_openapi::jiff::Timestamp::now());
        let record = LeaderElectionRecord {
            leader_transitions: old_record.leader_transitions,
            lease_duration_seconds: 1,
            renew_time: now_ts.clone(),
            acquire_time: now_ts,
            ..LeaderElectionRecord::default()
        };

        if let Err(err) = self.config.lock.update(&record).await {
            tracing::info!(lock = %desc, %err, "failed to release lease");
            return;
        }

        self.set_observed_record(&record).await;
    }

    /// Store a new observed record and update the observed time to now.
    async fn set_observed_record(&self, record: &LeaderElectionRecord) {
        let mut state = self.observed.write().await;
        state.record = record.clone();
        state.time = Instant::now();
    }

    /// Store a new observed record along with its raw bytes and update the
    /// observed time to now.
    async fn set_observed_record_with_raw(&self, record: &LeaderElectionRecord, raw: Vec<u8>) {
        let mut state = self.observed.write().await;
        state.record = record.clone();
        state.raw = raw;
        state.time = Instant::now();
    }

    /// Return a clone of the current observed record.
    async fn get_observed_record(&self) -> LeaderElectionRecord {
        self.observed.read().await.record.clone()
    }

    /// If the observed leader has changed since last report, invoke the
    /// `on_new_leader` callback.
    async fn maybe_report_transition(&self) {
        let holder = self.observed.read().await.record.holder_identity.clone();

        let mut reported = self.reported_leader.lock().await;
        if *reported == holder {
            return;
        }
        *reported = holder.clone();
        drop(reported);

        if let Some(ref cb) = self.config.callbacks.on_new_leader {
            let cb = Arc::clone(cb);
            tokio::spawn(async move { cb(&holder) });
        }
    }

    /// Returns `true` if the last observed leader is this instance.
    pub async fn is_leader(&self) -> bool {
        self.get_observed_record().await.holder_identity == self.config.lock.identity()
    }

    /// Returns the identity of the last observed leader.
    pub async fn get_leader(&self) -> String {
        self.get_observed_record().await.holder_identity
    }
}

/// Apply jitter to a duration.
///
/// Returns a duration in `[base, base * (1.0 + JITTER_FACTOR))`.
fn jittered_duration(base: Duration) -> Duration {
    let extra = rand::rng().random_range(0.0..JITTER_FACTOR);
    Duration::from_secs_f64(base.as_secs_f64() * (1.0 + extra))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LeaderCallbacks, LeaderElectionConfig};
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;

    // Use std::sync::Mutex for the mock so we can call `with_record` from
    // synchronous test setup without hitting "cannot block in async" errors.
    // The locks are never held across await points.
    type StdMutex<T> = std::sync::Mutex<T>;

    //MockLock
    //

    struct MockLock {
        identity: String,
        record: Arc<StdMutex<Option<LeaderElectionRecord>>>,
        raw: Arc<StdMutex<Vec<u8>>>,
        fail_next_get: Arc<AtomicBool>,
        fail_next_create: Arc<AtomicBool>,
        fail_next_update: Arc<AtomicBool>,
        always_fail_get: Arc<AtomicBool>,
        always_fail_update: Arc<AtomicBool>,
        fail_update_count: Arc<AtomicU32>,
        release_attempted: Arc<AtomicBool>,
    }

    impl MockLock {
        fn new(identity: &str) -> Self {
            Self {
                identity: identity.to_owned(),
                record: Arc::new(StdMutex::new(None)),
                raw: Arc::new(StdMutex::new(Vec::new())),
                fail_next_get: Arc::new(AtomicBool::new(false)),
                fail_next_create: Arc::new(AtomicBool::new(false)),
                fail_next_update: Arc::new(AtomicBool::new(false)),
                always_fail_get: Arc::new(AtomicBool::new(false)),
                always_fail_update: Arc::new(AtomicBool::new(false)),
                fail_update_count: Arc::new(AtomicU32::new(0)),
                release_attempted: Arc::new(AtomicBool::new(false)),
            }
        }

        fn with_record(self, record: LeaderElectionRecord) -> Self {
            let raw = serde_json::to_vec(&record).expect("mock record must be serializable");
            *self.record.lock().expect("test lock must not be poisoned") = Some(record);
            *self.raw.lock().expect("test lock must not be poisoned") = raw;
            self
        }
    }

    impl ResourceLock for MockLock {
        async fn get(&self) -> Result<(LeaderElectionRecord, Vec<u8>), LockError> {
            if self.always_fail_get.load(Ordering::SeqCst)
                || self.fail_next_get.swap(false, Ordering::SeqCst)
            {
                return Err(LockError::Conflict);
            }
            let guard = self.record.lock().expect("poisoned");
            match guard.as_ref() {
                Some(rec) => {
                    let raw = serde_json::to_vec(rec).map_err(LockError::Serialization)?;
                    Ok((rec.clone(), raw))
                }
                None => Err(LockError::NotFound),
            }
        }

        async fn create(&self, record: &LeaderElectionRecord) -> Result<(), LockError> {
            if self.fail_next_create.swap(false, Ordering::SeqCst) {
                return Err(LockError::Conflict);
            }
            let mut guard = self.record.lock().expect("poisoned");
            if guard.is_some() {
                return Err(LockError::Conflict);
            }
            let raw = serde_json::to_vec(record).map_err(LockError::Serialization)?;
            *guard = Some(record.clone());
            drop(guard);
            *self.raw.lock().expect("poisoned") = raw;
            Ok(())
        }

        async fn update(&self, record: &LeaderElectionRecord) -> Result<(), LockError> {
            if record.holder_identity.is_empty() {
                self.release_attempted.store(true, Ordering::SeqCst);
            }
            if self.always_fail_update.load(Ordering::SeqCst)
                || self.fail_next_update.swap(false, Ordering::SeqCst)
            {
                return Err(LockError::Conflict);
            }
            let count = self.fail_update_count.load(Ordering::SeqCst);
            if count > 0 {
                self.fail_update_count.fetch_sub(1, Ordering::SeqCst);
                return Err(LockError::Conflict);
            }
            let mut guard = self.record.lock().expect("poisoned");
            if guard.is_none() {
                return Err(LockError::NotInitialized);
            }
            let raw = serde_json::to_vec(record).map_err(LockError::Serialization)?;
            *guard = Some(record.clone());
            drop(guard);
            *self.raw.lock().expect("poisoned") = raw;
            Ok(())
        }

        fn identity(&self) -> &str {
            &self.identity
        }

        fn describe(&self) -> String {
            format!("mock/{}", self.identity)
        }
    }

    //Helpers
    //

    fn test_config(lock: MockLock) -> LeaderElectionConfig<MockLock> {
        LeaderElectionConfig {
            lock,
            lease_duration: Duration::from_secs(15),
            renew_deadline: Duration::from_secs(10),
            retry_period: Duration::from_secs(2),
            release_on_cancel: false,
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

    fn now_ts() -> String {
        format!("{:.6}", k8s_openapi::jiff::Timestamp::now())
    }

    //Tests
    //

    #[tokio::test(start_paused = true)]
    async fn acquire_on_empty_creates_lease() {
        let lock = MockLock::new("node-1");
        let record_ref = Arc::clone(&lock.record);

        let started = Arc::new(AtomicBool::new(false));
        let started_clone = Arc::clone(&started);

        let mut cfg = test_config(lock);
        cfg.callbacks.on_started_leading = Box::new(move |token| {
            let flag = Arc::clone(&started_clone);
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
                token.cancelled().await;
            })
        });

        let elector = LeaderElector::new(cfg).expect("valid config");
        let token = CancellationToken::new();
        let cancel = token.clone();

        let handle = tokio::spawn(async move {
            elector.run(token).await;
        });

        // Advance time past retry period to allow acquire loop to run.
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;

        // Verify lease was created with our identity.
        {
            let rec = record_ref.lock().expect("poisoned");
            assert!(rec.is_some());
            assert_eq!(rec.as_ref().unwrap().holder_identity, "node-1");
        }

        // Verify on_started_leading was called.
        assert!(started.load(Ordering::SeqCst));

        cancel.cancel();
        handle.await.expect("elector task should complete");
    }

    #[tokio::test(start_paused = true)]
    async fn cannot_acquire_held_lease() {
        let ts = now_ts();
        let existing = LeaderElectionRecord {
            holder_identity: "other-node".to_owned(),
            lease_duration_seconds: 15,
            acquire_time: ts.clone(),
            renew_time: ts,
            leader_transitions: 0,
            ..LeaderElectionRecord::default()
        };
        let lock = MockLock::new("node-1").with_record(existing);

        let cfg = test_config(lock);
        let elector = LeaderElector::new(cfg).expect("valid config");

        // Directly call try_acquire_or_renew — the lease is held and not expired.
        let result = elector.try_acquire_or_renew().await;
        assert!(!result, "should not acquire a held, unexpired lease");
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_expired_lease() {
        let ts = now_ts();
        let existing = LeaderElectionRecord {
            holder_identity: "other-node".to_owned(),
            lease_duration_seconds: 15,
            acquire_time: ts.clone(),
            renew_time: ts,
            leader_transitions: 5,
            ..LeaderElectionRecord::default()
        };
        let lock = MockLock::new("node-1").with_record(existing);
        let record_ref = Arc::clone(&lock.record);

        let cfg = test_config(lock);
        let elector = LeaderElector::new(cfg).expect("valid config");

        // First attempt observes the record and sets observed time to "now".
        // The lease is still valid, so this should fail.
        let result = elector.try_acquire_or_renew().await;
        assert!(!result, "should not acquire a held, unexpired lease");

        // Advance time past lease duration so the observed lease expires.
        tokio::time::advance(Duration::from_secs(16)).await;

        // Second attempt should succeed because the lease has expired since
        // the last observation.
        let result = elector.try_acquire_or_renew().await;
        assert!(result, "should acquire an expired lease");

        let rec = record_ref.lock().expect("poisoned");
        let rec = rec.as_ref().expect("record should exist");
        assert_eq!(rec.holder_identity, "node-1");
        assert_eq!(rec.leader_transitions, 6, "transitions should increment");
    }

    #[tokio::test(start_paused = true)]
    async fn renew_as_leader_succeeds_via_fast_path() {
        // Set ourselves as the leader with a valid lease.
        let ts = now_ts();
        let existing = LeaderElectionRecord {
            holder_identity: "node-1".to_owned(),
            lease_duration_seconds: 15,
            acquire_time: ts.clone(),
            renew_time: ts,
            leader_transitions: 1,
            ..LeaderElectionRecord::default()
        };
        let lock = MockLock::new("node-1").with_record(existing.clone());

        let cfg = test_config(lock);
        let elector = LeaderElector::new(cfg).expect("valid config");

        // Prime the observed record so the elector knows it's the leader.
        elector.set_observed_record(&existing).await;

        let result = elector.try_acquire_or_renew().await;
        assert!(result, "should renew as leader via fast path");

        // Transitions should not change on renewal.
        let rec = elector.get_observed_record().await;
        assert_eq!(rec.leader_transitions, 1);
        assert_eq!(rec.holder_identity, "node-1");
    }

    #[tokio::test(start_paused = true)]
    async fn fast_path_fallback_on_update_failure() {
        let ts = now_ts();
        let existing = LeaderElectionRecord {
            holder_identity: "node-1".to_owned(),
            lease_duration_seconds: 15,
            acquire_time: ts.clone(),
            renew_time: ts,
            leader_transitions: 1,
            ..LeaderElectionRecord::default()
        };
        let lock = MockLock::new("node-1").with_record(existing.clone());

        // The first update (fast path) will fail, forcing a fallback to GET+UPDATE.
        lock.fail_next_update.store(true, Ordering::SeqCst);

        let cfg = test_config(lock);
        let elector = LeaderElector::new(cfg).expect("valid config");
        elector.set_observed_record(&existing).await;

        let result = elector.try_acquire_or_renew().await;
        assert!(
            result,
            "should succeed via slow path after fast path failure"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn leadership_lost_when_renewal_fails_past_deadline() {
        let ts = now_ts();
        let existing = LeaderElectionRecord {
            holder_identity: "node-1".to_owned(),
            lease_duration_seconds: 15,
            acquire_time: ts.clone(),
            renew_time: ts,
            leader_transitions: 0,
            ..LeaderElectionRecord::default()
        };
        let lock = MockLock::new("node-1").with_record(existing.clone());
        let always_fail_update = Arc::clone(&lock.always_fail_update);
        let always_fail_get = Arc::clone(&lock.always_fail_get);

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_clone = Arc::clone(&stopped);

        let started = Arc::new(AtomicBool::new(false));
        let started_clone = Arc::clone(&started);

        let mut cfg = test_config(lock);
        cfg.callbacks.on_started_leading = Box::new(move |token| {
            let flag = Arc::clone(&started_clone);
            Box::pin(async move {
                flag.store(true, Ordering::SeqCst);
                token.cancelled().await;
            })
        });
        cfg.callbacks.on_stopped_leading = Box::new(move || {
            stopped_clone.store(true, Ordering::SeqCst);
        });

        let elector = LeaderElector::new(cfg).expect("valid config");
        let token = CancellationToken::new();

        let handle = tokio::spawn(async move {
            elector.run(token).await;
        });

        // Let the elector acquire and start leading.
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;
        assert!(
            started.load(Ordering::SeqCst),
            "should have started leading"
        );

        // Make all updates and gets fail permanently.
        always_fail_update.store(true, Ordering::SeqCst);
        always_fail_get.store(true, Ordering::SeqCst);

        // Advance past renew_deadline (10s) + buffer to trigger leadership loss.
        for _ in 0..20 {
            tokio::time::advance(Duration::from_secs(2)).await;
            tokio::task::yield_now().await;
        }

        handle.await.expect("elector task should complete");

        assert!(
            stopped.load(Ordering::SeqCst),
            "on_stopped_leading should have been called"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn on_new_leader_fires_on_transition() {
        let leaders: Arc<std::sync::Mutex<Vec<String>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let leaders_clone = Arc::clone(&leaders);

        let lock = MockLock::new("node-1");

        let mut cfg = test_config(lock);
        cfg.callbacks.on_new_leader = Some(Arc::new(move |leader: &str| {
            leaders_clone
                .lock()
                .expect("lock poisoned")
                .push(leader.to_owned());
        }));

        let elector = LeaderElector::new(cfg).expect("valid config");
        let token = CancellationToken::new();
        let cancel = token.clone();

        let handle = tokio::spawn(async move {
            elector.run(token).await;
        });

        // Let it acquire.
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;

        cancel.cancel();
        handle.await.expect("elector task should complete");

        let observed = leaders.lock().expect("lock poisoned");
        assert!(
            observed.contains(&"node-1".to_owned()),
            "on_new_leader should have been called with our identity"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn on_stopped_leading_fires_on_cancellation() {
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_clone = Arc::clone(&stopped);

        let lock = MockLock::new("node-1");
        let mut cfg = test_config(lock);
        cfg.callbacks.on_stopped_leading = Box::new(move || {
            stopped_clone.store(true, Ordering::SeqCst);
        });

        let elector = LeaderElector::new(cfg).expect("valid config");
        let token = CancellationToken::new();

        // Cancel immediately before running.
        token.cancel();

        elector.run(token).await;

        assert!(
            stopped.load(Ordering::SeqCst),
            "on_stopped_leading must fire even when cancelled before acquiring"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn release_on_cancel_clears_holder() {
        let lock = MockLock::new("node-1");
        let record_ref = Arc::clone(&lock.record);

        let mut cfg = test_config(lock);
        cfg.release_on_cancel = true;

        let elector = LeaderElector::new(cfg).expect("valid config");
        let token = CancellationToken::new();
        let cancel = token.clone();

        let handle = tokio::spawn(async move {
            elector.run(token).await;
        });

        // Let the elector acquire.
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;

        // Verify we are the leader.
        {
            let rec = record_ref.lock().expect("poisoned");
            assert_eq!(rec.as_ref().unwrap().holder_identity, "node-1");
        }

        // Cancel and wait for release.
        cancel.cancel();
        // Advance time to allow release to happen (within renew_deadline timeout).
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::task::yield_now().await;

        handle.await.expect("elector task should complete");

        // After release, holder should be empty and lease_duration should be 1.
        let rec = record_ref.lock().expect("poisoned");
        let rec = rec.as_ref().expect("record should exist");
        assert!(
            rec.holder_identity.is_empty(),
            "holder should be cleared on release"
        );
        assert_eq!(rec.lease_duration_seconds, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn release_attempted_on_renewal_failure() {
        let lock = MockLock::new("node-1");
        let record_ref = Arc::clone(&lock.record);
        let always_fail_update = Arc::clone(&lock.always_fail_update);
        let release_attempted = Arc::clone(&lock.release_attempted);

        let mut cfg = test_config(lock);
        cfg.release_on_cancel = true;

        let elector = LeaderElector::new(cfg).expect("valid config");
        let token = CancellationToken::new();

        let handle = tokio::spawn(async move {
            elector.run(token).await;
        });

        // Let the elector acquire and start leading.
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;

        // Verify we are the leader.
        {
            let rec = record_ref.lock().expect("poisoned");
            assert_eq!(rec.as_ref().unwrap().holder_identity, "node-1");
        }

        // Make updates fail permanently to force renewal failure.
        // Gets remain working so release_inner can read the current record.
        always_fail_update.store(true, Ordering::SeqCst);

        // Advance past renew_deadline to trigger leadership loss.
        for _ in 0..20 {
            tokio::time::advance(Duration::from_secs(2)).await;
            tokio::task::yield_now().await;
        }

        handle.await.expect("elector task should complete");

        // Verify that release was attempted (update with empty holder).
        // The release itself may fail because the API is down, but the
        // important thing is that the code path was exercised.
        assert!(
            release_attempted.load(Ordering::SeqCst),
            "release should be attempted on renewal failure when release_on_cancel is true"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn no_release_on_renewal_failure_without_flag() {
        let lock = MockLock::new("node-1");
        let always_fail_update = Arc::clone(&lock.always_fail_update);
        let release_attempted = Arc::clone(&lock.release_attempted);

        let cfg = test_config(lock);
        // release_on_cancel defaults to false in test_config

        let elector = LeaderElector::new(cfg).expect("valid config");
        let token = CancellationToken::new();

        let handle = tokio::spawn(async move {
            elector.run(token).await;
        });

        // Let the elector acquire and start leading.
        tokio::time::advance(Duration::from_secs(3)).await;
        tokio::task::yield_now().await;

        // Make updates fail permanently to force renewal failure.
        always_fail_update.store(true, Ordering::SeqCst);

        // Advance past renew_deadline to trigger leadership loss.
        for _ in 0..20 {
            tokio::time::advance(Duration::from_secs(2)).await;
            tokio::task::yield_now().await;
        }

        handle.await.expect("elector task should complete");

        assert!(
            !release_attempted.load(Ordering::SeqCst),
            "release should NOT be attempted when release_on_cancel is false"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_config_rejected() {
        let lock = MockLock::new("node-1");
        let cfg = LeaderElectionConfig {
            lock,
            lease_duration: Duration::from_secs(5),
            renew_deadline: Duration::from_secs(10),
            retry_period: Duration::from_secs(2),
            release_on_cancel: false,
            callbacks: LeaderCallbacks {
                on_started_leading: Box::new(|token| {
                    Box::pin(async move {
                        token.cancelled().await;
                    })
                }),
                on_stopped_leading: Box::new(|| {}),
                on_new_leader: None,
            },
        };

        let result = LeaderElector::new(cfg);
        assert!(result.is_err(), "should reject invalid config");
    }
}
