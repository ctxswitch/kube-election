# kube-election

[![CI](https://github.com/ctxswitch/kube-election/workflows/ci/badge.svg)](https://github.com/ctxswitch/kube-election/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/kube-election.svg)](https://crates.io/crates/kube-election)
[![Documentation](https://docs.rs/kube-election/badge.svg)](https://docs.rs/kube-election)
[![codecov](https://img.shields.io/codecov/c/github/ctxswitch/kube-election)](https://app.codecov.io/gh/ctxswitch/kube-election)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

A Kubernetes leader election library using Lease objects that closely mirrors the feature sets of the controller-runtime package in the Go ecosystem.

## Usage

```rust
use std::sync::Arc;
use k8s_openapi::api::apps::v1::Deployment;
use kube::{Api, Client, runtime::controller::Action};
use kube::runtime::Controller;
use kube::runtime::watcher::Config;
use kube_election::LeaderElection;
use tokio_util::sync::CancellationToken;

struct Ctx {
    client: Client,
}

async fn reconcile(obj: Arc<Deployment>, ctx: Arc<Ctx>) -> Result<Action, kube::Error> {
    let name = obj.metadata.name.as_deref().unwrap_or("<unknown>");
    tracing::info!(%name, "reconciling deployment");
    Ok(Action::requeue(std::time::Duration::from_secs(300)))
}

fn error_policy(_obj: Arc<Deployment>, err: &kube::Error, _ctx: Arc<Ctx>) -> Action {
    tracing::error!(%err, "reconcile error");
    Action::requeue(std::time::Duration::from_secs(60))
}

async fn run_controller(client: Client, token: CancellationToken) {
    let deployments = Api::<Deployment>::all(client.clone());
    let ctx = Arc::new(Ctx { client });

    Controller::new(deployments, Config::default())
        .graceful_shutdown_on(token.cancelled_owned())
        .run(reconcile, error_policy, ctx)
        .for_each(|_| futures::future::ready(()))
        .await;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::try_default().await?;

    let election = LeaderElection::builder("my-controller-leader", client.clone())
        .release_on_cancel(true)
        .on_started_leading(move |token| {
            let client = client.clone();
            async move { run_controller(client, token).await }
        })
        .on_stopped_leading(|| {
            tracing::warn!("lost leadership, shutting down");
        })
        .build()?;

    // Cancel the election on SIGTERM/SIGINT for graceful shutdown.
    let token = CancellationToken::new();
    let shutdown = token.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        shutdown.cancel();
    });

    election.run(token).await;
    // At this point the controller has fully stopped, the lease is
    // released, and on_stopped_leading has been called.
    Ok(())
}
```

## Builder options

| Option | Default | Description |
|---|---|---|
| **namespace** | in-cluster service account | Namespace of the Lease resource. Auto-detected from the pod's service account mount. |
| **identity** | `{hostname}_{uuid}` | Unique identity for this candidate. Auto-generated to ensure uniqueness across replicas. |
| **lease_duration** | 15s | How long a non-leader waits after observing the lease before attempting to acquire it. Must be greater than `renew_deadline`. |
| **renew_deadline** | 10s | How long the leader retries refreshing the lease before giving up and losing leadership. |
| **retry_period** | 2s | Interval between acquire or renew attempts. Jitter is applied automatically. |
| **release_on_cancel** | false | Whether to release the lease when the cancellation token fires. Speeds up leader transitions but requires the process to exit immediately after `run` returns. |
| **shutdown_grace_period** | 30s | Maximum time to wait for return after leadership is lost. If the callback does not exit within this period the task is aborted. |
| **on_started_leading** | wait for cancellation | Called when this instance becomes the leader. Receives a `CancellationToken` that is cancelled when leadership is lost. |
| **on_stopped_leading** | no-op | Called when this instance stops leading. Always called before `run` returns, even if leadership was never acquired. |
| **on_new_leader** | none | Called when a new leader is observed, receiving the leader's identity string. |

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
