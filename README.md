# kube-election

A Kubernetes leader election library using Lease objects, ported 1:1 from Go's
[client-go/tools/leaderelection](https://github.com/kubernetes/client-go/tree/master/tools/leaderelection).

Only one replica in a set reconciles at a time, matching the pattern used by
Go's controller-runtime.

## Usage

```rust
use kube_election::{
    LeaderElector, LeaderElectionConfig, LeaderCallbacks, LeaseLock,
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

let client = kube::Client::try_default().await?;
let lock = LeaseLock::new(
    client,
    "my-controller-leader".into(),
    "default".into(),
    "pod-abc123".into(),
);

let config = LeaderElectionConfig {
    lock,
    lease_duration: Duration::from_secs(15),
    renew_deadline: Duration::from_secs(10),
    retry_period: Duration::from_secs(2),
    release_on_cancel: true,
    name: "my-controller".into(),
    callbacks: LeaderCallbacks {
        on_started_leading: Box::new(|token| Box::pin(async move {
            // Run your controller here. The token is cancelled
            // when leadership is lost.
            my_controller(token).await;
        })),
        on_stopped_leading: Box::new(|| {
            tracing::info!("stopped leading");
        }),
        on_new_leader: Some(std::sync::Arc::new(|identity: &str| {
            tracing::info!(%identity, "new leader observed");
        })),
    },
};

let elector = LeaderElector::new(config)?;
let token = CancellationToken::new();
elector.run(token).await;
// on_stopped_leading is always called before run returns,
// even if leadership was never acquired.
```

## Design

### Clock skew tolerance

Lease validity is determined using local monotonic time (`tokio::time::Instant`),
never server timestamps. The implementation does not depend on timestamp accuracy
across nodes and is tolerant to arbitrary clock skew (but not arbitrary skew
*rate*). Configure `lease_duration` and `renew_deadline` to match your tolerance.

### Atomic compare-and-swap

`LeaseLock` uses `kube::Api::replace()` (HTTP PUT with `resourceVersion`) for
updates. The API server rejects concurrent writes via 409 Conflict, providing
the atomic CAS guarantee without merge patches.

### Two-phase acquisition

Leaders take a fast path: optimistically update the lease without a GET,
assuming the last observed record is current. On conflict, fall back to the
slow path (GET then PUT). Non-leaders always use the slow path.

### Jitter

Retry sleeps include jitter matching Go's `wait.Jitter(duration, 1.2)`,
producing intervals in `[base, base * 2.2)` to prevent thundering herds.

## Configuration invariants

These are validated by `LeaderElector::new()`:

- `lease_duration > renew_deadline`
- `renew_deadline > retry_period * 1.2`
- All durations must be non-zero
- Lock identity must not be empty

## Custom locks

Implement the `ResourceLock` trait to back elections with a resource other than
Lease:

```rust
use kube_election::{ResourceLock, LeaderElectionRecord, LockError};

struct MyLock { /* ... */ }

impl ResourceLock for MyLock {
    async fn get(&self) -> Result<(LeaderElectionRecord, Vec<u8>), LockError> { todo!() }
    async fn create(&self, record: &LeaderElectionRecord) -> Result<(), LockError> { todo!() }
    async fn update(&self, record: &LeaderElectionRecord) -> Result<(), LockError> { todo!() }
    fn identity(&self) -> &str { todo!() }
    fn describe(&self) -> String { todo!() }
}
```

## Go to Rust mapping

| Go | Rust | Notes |
|---|---|---|
| `context.Context` | `CancellationToken` | Async cancellation |
| `go func()` | `tokio::spawn` | Detached task for `on_started_leading` |
| `defer` | Explicit call at all exit paths | `on_stopped_leading` always fires |
| `sync.RWMutex` | `tokio::sync::RwLock` | Single lock guards observed state |
| `clock.Clock` | `tokio::time::Instant` | `pause()` enables deterministic tests |
| `Update` (PUT) | `kube::Api::replace()` | Same HTTP PUT with resourceVersion |
| `bytes.Equal` | `Vec<u8>` equality | Raw record comparison |
| `wait.JitterUntil` | Loop + `sleep(jittered)` | Manual jitter implementation |
