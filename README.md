# kube-election

A Kubernetes leader election library using Lease objects that closely mirrors the feature sets of the controller-runtime package in the go ecosystem.

Only one replica in a set reconciles at a time, ensuring single-writer semantics for controllers.

## Usage

```rust
use kube_election::LeaderElection;
use tokio_util::sync::CancellationToken;

let client = kube::Client::try_default().await?;
let election = LeaderElection::builder("my-controller-leader", client)
    .on_started_leading(|token| async move {
        // Run your controller here. The token is cancelled
        // when leadership is lost.
        my_controller(token).await;
    })
    .on_stopped_leading(|| {
        tracing::info!("stopped leading");
    })
    .on_new_leader(|identity| {
        tracing::info!(%identity, "new leader observed");
    })
    .build()?;

let token = CancellationToken::new();
election.run(token).await;
// on_stopped_leading is always called before run returns,
// even if leadership was never acquired.
```

Builder defaults:

| Option | Default |
|---|---|
| **namespace** | read from the in-cluster service account |
| **identity** | `{hostname}_{uuid}` |
| **lease_duration** | 15s |
| **renew_deadline** | 10s |
| **retry_period** | 2s |
| **release_on_cancel** | false |

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

Retry sleeps include jitter producing intervals in `[base, base * 2.2)` to
prevent thundering herds.

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
