use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::{LeaderCallbacks, LeaderElectionConfig};
use crate::elector::LeaderElector;
use crate::error::LeaderElectionError;
use crate::lock::{generate_identity, in_cluster_namespace, LeaseLock};

const DEFAULT_LEASE_DURATION: Duration = Duration::from_secs(15);
const DEFAULT_RENEW_DEADLINE: Duration = Duration::from_secs(10);
const DEFAULT_RETRY_PERIOD: Duration = Duration::from_secs(2);
const DEFAULT_SHUTDOWN_GRACE_PERIOD: Duration = Duration::from_secs(30);

/// High-level leader election handle.
///
/// Wraps the lower-level `LeaderElector` and `LeaseLock` behind a single
/// configuration surface with sensible defaults.
///
/// # Example
///
/// ```rust,no_run
/// use kube_election::LeaderElection;
/// use tokio_util::sync::CancellationToken;
///
/// # async fn run() -> Result<(), Box<dyn std::error::Error>> {
/// let client = kube::Client::try_default().await?;
/// let election = LeaderElection::builder("my-controller-leader", client)
///     .on_started_leading(|token| Box::pin(async move {
///         token.cancelled().await;
///     }))
///     .on_stopped_leading(|| {
///         eprintln!("stopped leading");
///     })
///     .build()?;
///
/// let token = CancellationToken::new();
/// election.run(token).await;
/// # Ok(())
/// # }
/// ```
pub struct LeaderElection {
    elector: LeaderElector<LeaseLock>,
}

impl LeaderElection {
    /// Create a builder for a leader election instance.
    ///
    /// `name` is the name of the Lease resource that replicas compete over.
    pub fn builder(name: impl Into<String>, client: kube::Client) -> LeaderElectionBuilder {
        LeaderElectionBuilder {
            name: name.into(),
            client,
            namespace: None,
            identity: None,
            lease_duration: DEFAULT_LEASE_DURATION,
            renew_deadline: DEFAULT_RENEW_DEADLINE,
            retry_period: DEFAULT_RETRY_PERIOD,
            release_on_cancel: false,
            shutdown_grace_period: DEFAULT_SHUTDOWN_GRACE_PERIOD,
            on_started_leading: None,
            on_stopped_leading: None,
            on_new_leader: None,
        }
    }

    /// Run the leader election loop.
    ///
    /// Runs until the cancellation token fires. If leadership is held and
    /// renewal fails past `renew_deadline`, the loop exits.
    /// `on_stopped_leading` is always called before this returns.
    pub async fn run(self, token: CancellationToken) {
        self.elector.run(token).await;
    }
}

type OnStartedLeading =
    Box<dyn Fn(CancellationToken) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
type OnNewLeader = Arc<dyn Fn(&str) + Send + Sync>;

/// Builder for [`LeaderElection`].
///
/// Defaults:
/// - **namespace**: read from the in-cluster service account
/// - **identity**: `{hostname}_{uuid}`
/// - **lease_duration**: 15s
/// - **renew_deadline**: 10s
/// - **retry_period**: 2s
/// - **release_on_cancel**: false
pub struct LeaderElectionBuilder {
    name: String,
    client: kube::Client,
    namespace: Option<String>,
    identity: Option<String>,
    lease_duration: Duration,
    renew_deadline: Duration,
    retry_period: Duration,
    release_on_cancel: bool,
    shutdown_grace_period: Duration,
    on_started_leading: Option<OnStartedLeading>,
    on_stopped_leading: Option<Box<dyn FnOnce() + Send + Sync>>,
    on_new_leader: Option<OnNewLeader>,
}

impl LeaderElectionBuilder {
    /// Override the namespace. Defaults to the in-cluster service account
    /// namespace.
    pub fn namespace(mut self, ns: impl Into<String>) -> Self {
        self.namespace = Some(ns.into());
        self
    }

    /// Override the holder identity. Defaults to `{hostname}_{uuid}`.
    pub fn identity(mut self, id: impl Into<String>) -> Self {
        self.identity = Some(id.into());
        self
    }

    /// How long a non-leader waits before attempting to acquire the lease
    /// after observing it. Default: 15s.
    pub fn lease_duration(mut self, d: Duration) -> Self {
        self.lease_duration = d;
        self
    }

    /// How long the leader retries refreshing before giving up.
    /// Default: 10s.
    pub fn renew_deadline(mut self, d: Duration) -> Self {
        self.renew_deadline = d;
        self
    }

    /// Interval between acquire/renew attempts. Default: 2s.
    pub fn retry_period(mut self, d: Duration) -> Self {
        self.retry_period = d;
        self
    }

    /// Whether to release the lease when the cancellation token fires.
    /// Default: false.
    pub fn release_on_cancel(mut self, v: bool) -> Self {
        self.release_on_cancel = v;
        self
    }

    /// Maximum time to wait for `on_started_leading` to return after
    /// leadership is lost before aborting the task. Default: 30s.
    pub fn shutdown_grace_period(mut self, d: Duration) -> Self {
        self.shutdown_grace_period = d;
        self
    }

    /// Called when this instance becomes the leader. Receives a token that
    /// is cancelled when leadership is lost.
    pub fn on_started_leading<F, Fut>(mut self, f: F) -> Self
    where
        F: Fn(CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.on_started_leading = Some(Box::new(move |token| Box::pin(f(token))));
        self
    }

    /// Called when this instance stops leading. Always called before
    /// [`LeaderElection::run`] returns, even if leadership was never
    /// acquired.
    pub fn on_stopped_leading<F>(mut self, f: F) -> Self
    where
        F: FnOnce() + Send + Sync + 'static,
    {
        self.on_stopped_leading = Some(Box::new(f));
        self
    }

    /// Called when a new leader is observed.
    pub fn on_new_leader<F>(mut self, f: F) -> Self
    where
        F: Fn(&str) + Send + Sync + 'static,
    {
        self.on_new_leader = Some(Arc::new(f));
        self
    }

    /// Build the [`LeaderElection`] instance.
    ///
    /// Returns an error if the namespace or identity cannot be determined,
    /// or if the configuration is invalid.
    pub fn build(self) -> Result<LeaderElection, LeaderElectionError> {
        let namespace = match self.namespace {
            Some(ns) => ns,
            None => in_cluster_namespace()
                .map_err(|e| LeaderElectionError::InvalidConfig(Box::new(e)))?,
        };

        let identity = match self.identity {
            Some(id) => id,
            None => {
                generate_identity().map_err(|e| LeaderElectionError::InvalidConfig(Box::new(e)))?
            }
        };

        let lock = LeaseLock::with_options(self.client, self.name, namespace, identity);

        let on_started_leading = self.on_started_leading.unwrap_or_else(|| {
            Box::new(|token: CancellationToken| {
                Box::pin(async move {
                    token.cancelled().await;
                })
            })
        });

        let config = LeaderElectionConfig {
            lock,
            lease_duration: self.lease_duration,
            renew_deadline: self.renew_deadline,
            retry_period: self.retry_period,
            release_on_cancel: self.release_on_cancel,
            shutdown_grace_period: self.shutdown_grace_period,
            callbacks: LeaderCallbacks {
                on_started_leading,
                on_stopped_leading: self.on_stopped_leading.unwrap_or_else(|| Box::new(|| {})),
                on_new_leader: self.on_new_leader,
            },
        };

        let elector = LeaderElector::new(config)?;
        Ok(LeaderElection { elector })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_client() -> kube::Client {
        rustls::crypto::aws_lc_rs::default_provider()
            .install_default()
            .ok();
        let config = kube::Config::new("http://localhost:1".parse().unwrap());
        kube::Client::try_from(config).unwrap()
    }

    #[tokio::test]
    async fn build_with_explicit_namespace_and_identity() {
        let election = LeaderElection::builder("test-lease", test_client())
            .namespace("default")
            .identity("node-1")
            .build();
        assert!(election.is_ok());
    }

    #[tokio::test]
    async fn build_with_all_options() {
        let election = LeaderElection::builder("test-lease", test_client())
            .namespace("kube-system")
            .identity("node-2")
            .lease_duration(Duration::from_secs(30))
            .renew_deadline(Duration::from_secs(20))
            .retry_period(Duration::from_secs(5))
            .release_on_cancel(true)
            .on_started_leading(|token| async move {
                token.cancelled().await;
            })
            .on_stopped_leading(|| {})
            .on_new_leader(|_identity| {})
            .build();
        assert!(election.is_ok());
    }

    #[tokio::test]
    async fn build_rejects_invalid_config() {
        let result = LeaderElection::builder("test-lease", test_client())
            .namespace("default")
            .identity("node-1")
            .lease_duration(Duration::ZERO)
            .build();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn build_rejects_empty_identity() {
        let result = LeaderElection::builder("test-lease", test_client())
            .namespace("default")
            .identity("")
            .build();
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn build_without_namespace_fails_outside_cluster() {
        let result = LeaderElection::builder("test-lease", test_client())
            .identity("node-1")
            .build();
        assert!(
            result.is_err(),
            "should fail when namespace cannot be auto-detected"
        );
    }
}
