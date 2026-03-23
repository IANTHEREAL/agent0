// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

//! A utility module for managing and retrying PD requests.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tokio::time::sleep;

use crate::internal_err;
use crate::pd::Cluster;
use crate::pd::Connection;
use crate::proto::keyspacepb;
use crate::proto::metapb;
use crate::proto::pdpb::Timestamp;
use crate::proto::pdpb::{self};
use crate::region::RegionId;
use crate::region::RegionWithLeader;
use crate::region::StoreId;
use crate::stats::pd_stats;
use crate::Error;
use crate::Result;
use crate::SecurityManager;

// FIXME: these numbers and how they are used are all just cargo-culted in, there
// may be more optimal values.
const RECONNECT_INTERVAL_SEC: u64 = 1;
const MAX_REQUEST_COUNT: usize = 5;
const LEADER_CHANGE_RETRY: usize = 10;

#[async_trait]
pub trait RetryClientTrait {
    // These get_* functions will try multiple times to make a request, reconnecting as necessary.
    // It does not know about encoding. Caller should take care of it.
    async fn get_region(self: Arc<Self>, key: Vec<u8>) -> Result<RegionWithLeader>;

    async fn get_region_by_id(self: Arc<Self>, region_id: RegionId) -> Result<RegionWithLeader>;

    async fn get_store(self: Arc<Self>, id: StoreId) -> Result<metapb::Store>;

    async fn get_all_stores(self: Arc<Self>) -> Result<Vec<metapb::Store>>;

    async fn get_timestamp(self: Arc<Self>) -> Result<Timestamp>;

    async fn get_gc_safepoint(self: Arc<Self>) -> Result<u64>;

    async fn update_safepoint(self: Arc<Self>, safepoint: u64) -> Result<bool>;

    /// Register or refresh a per-service GC safe point with TTL lease.
    /// Setting `ttl_secs = 0` removes the service safe point.
    /// Returns the minimum safe point across all services.
    async fn update_service_safepoint(
        self: Arc<Self>,
        service_id: &str,
        ttl_secs: i64,
        safe_point: u64,
    ) -> Result<u64>;

    async fn load_keyspace(&self, keyspace: &str) -> Result<keyspacepb::KeyspaceMeta>;
}
/// Client for communication with a PD cluster. Has the facility to reconnect to the cluster.
pub struct RetryClient<Cl = Cluster> {
    // Tuple is the cluster and the time of the cluster's last reconnect.
    cluster: RwLock<(Cl, Instant)>,
    connection: Connection,
    timeout: Duration,
}

#[cfg(test)]
impl<Cl> RetryClient<Cl> {
    pub fn new_with_cluster(
        security_mgr: Arc<SecurityManager>,
        timeout: Duration,
        cluster: Cl,
    ) -> RetryClient<Cl> {
        let connection = Connection::new(security_mgr);
        RetryClient {
            cluster: RwLock::new((cluster, Instant::now())),
            connection,
            timeout,
        }
    }
}

macro_rules! retry_core {
    ($self: ident, $tag: literal, $call: expr) => {{
        let stats = pd_stats($tag);
        let mut last_err = Ok(());
        for _ in 0..LEADER_CHANGE_RETRY {
            let res = $call;

            match stats.done(res) {
                Ok(r) => return Ok(r),
                Err(e) => last_err = Err(e),
            }

            let mut reconnect_count = MAX_REQUEST_COUNT;
            while let Err(e) = $self.reconnect(RECONNECT_INTERVAL_SEC).await {
                reconnect_count -= 1;
                if reconnect_count == 0 {
                    return Err(e);
                }
                sleep(Duration::from_secs(RECONNECT_INTERVAL_SEC)).await;
            }
        }

        last_err?;
        unreachable!();
    }};
}

macro_rules! retry_mut {
    ($self: ident, $tag: literal, |$cluster: ident| $call: expr) => {{
        retry_core!($self, $tag, {
            // use the block here to drop the guard of the lock,
            // otherwise `reconnect` will try to acquire the write lock and results in a deadlock
            let $cluster = &mut $self.cluster.write().await.0;
            $call.await
        })
    }};
}

macro_rules! retry {
    ($self: ident, $tag: literal, |$cluster: ident| $call: expr) => {{
        retry_core!($self, $tag, {
            // use the block here to drop the guard of the lock,
            // otherwise `reconnect` will try to acquire the write lock and results in a deadlock
            let $cluster = &$self.cluster.read().await.0;
            $call.await
        })
    }};
}

macro_rules! retry_core_with_timeout {
    ($self: ident, $tag: literal, $timeout: expr, $call: expr) => {{
        let timeout = $timeout;
        tokio::time::timeout(timeout, async {
            let stats = pd_stats($tag);
            let mut last_err = Ok(());
            for _ in 0..LEADER_CHANGE_RETRY {
                let res = $call;

                match stats.done(res) {
                    Ok(r) => return Ok(r),
                    Err(e) => last_err = Err(e),
                }

                let mut reconnect_count = MAX_REQUEST_COUNT;
                while let Err(e) = $self.reconnect(RECONNECT_INTERVAL_SEC).await {
                    reconnect_count -= 1;
                    if reconnect_count == 0 {
                        return Err(e);
                    }
                    sleep(Duration::from_secs(RECONNECT_INTERVAL_SEC)).await;
                }
            }

            last_err?;
            unreachable!();
        })
        .await
        .map_err(|_| internal_err!("{} timed out after {:?}", $tag, timeout))?
    }};
}

impl RetryClient<Cluster> {
    pub async fn connect(
        endpoints: &[String],
        security_mgr: Arc<SecurityManager>,
        timeout: Duration,
    ) -> Result<RetryClient> {
        let connection = Connection::new(security_mgr);
        let cluster = RwLock::new((
            connection.connect_cluster(endpoints, timeout).await?,
            Instant::now(),
        ));
        Ok(RetryClient {
            cluster,
            connection,
            timeout,
        })
    }

    pub(crate) async fn get_timestamp_with_timeout(
        self: Arc<Self>,
        timeout: Duration,
    ) -> Result<Timestamp> {
        let deadline = Instant::now() + timeout;
        retry_core_with_timeout!(self, "get_timestamp_with_timeout", timeout, {
            // Cap each attempt to 2/3 of the original budget. This allows
            // slow-but-healthy PD responses (e.g. leader election ~10s) to
            // succeed on the first attempt, while still reserving 1/3 of the
            // total budget for at least one reconnect + retry cycle when PD
            // is truly hung.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let per_attempt = remaining.min(timeout * 2 / 3);
            let cluster = &self.cluster.read().await.0;
            cluster.get_timestamp_with_timeout(per_attempt).await
        })
    }
}

#[async_trait]
impl RetryClientTrait for RetryClient<Cluster> {
    // These get_* functions will try multiple times to make a request, reconnecting as necessary.
    // It does not know about encoding. Caller should take care of it.
    async fn get_region(self: Arc<Self>, key: Vec<u8>) -> Result<RegionWithLeader> {
        retry_mut!(self, "get_region", |cluster| {
            let key = key.clone();
            async {
                cluster
                    .get_region(key.clone(), self.timeout)
                    .await
                    .and_then(|resp| {
                        region_from_response(resp, || Error::RegionForKeyNotFound { key })
                    })
            }
        })
    }

    async fn get_region_by_id(self: Arc<Self>, region_id: RegionId) -> Result<RegionWithLeader> {
        retry_mut!(self, "get_region_by_id", |cluster| async {
            cluster
                .get_region_by_id(region_id, self.timeout)
                .await
                .and_then(|resp| {
                    region_from_response(resp, || Error::RegionNotFoundInResponse { region_id })
                })
        })
    }

    async fn get_store(self: Arc<Self>, id: StoreId) -> Result<metapb::Store> {
        retry_mut!(self, "get_store", |cluster| async {
            cluster
                .get_store(id, self.timeout)
                .await
                .map(|resp| resp.store.unwrap())
        })
    }

    async fn get_all_stores(self: Arc<Self>) -> Result<Vec<metapb::Store>> {
        retry_mut!(self, "get_all_stores", |cluster| async {
            cluster
                .get_all_stores(self.timeout)
                .await
                .map(|resp| resp.stores.into_iter().map(Into::into).collect())
        })
    }

    async fn get_timestamp(self: Arc<Self>) -> Result<Timestamp> {
        retry!(self, "get_timestamp", |cluster| cluster.get_timestamp())
    }

    async fn get_gc_safepoint(self: Arc<Self>) -> Result<u64> {
        retry_mut!(self, "get_gc_safepoint", |cluster| async {
            cluster
                .get_gc_safepoint(self.timeout)
                .await
                .map(|resp| resp.safe_point)
        })
    }

    async fn update_safepoint(self: Arc<Self>, safepoint: u64) -> Result<bool> {
        retry_mut!(self, "update_gc_safepoint", |cluster| async {
            cluster
                .update_safepoint(safepoint, self.timeout)
                .await
                .map(|resp| resp.new_safe_point == safepoint)
        })
    }

    async fn update_service_safepoint(
        self: Arc<Self>,
        service_id: &str,
        ttl_secs: i64,
        safe_point: u64,
    ) -> Result<u64> {
        let sid = service_id.to_owned();
        retry_mut!(self, "update_service_gc_safepoint", |cluster| async {
            cluster
                .update_service_safepoint(&sid, ttl_secs, safe_point, self.timeout)
                .await
                .map(|resp| resp.min_safe_point)
        })
    }

    async fn load_keyspace(&self, keyspace: &str) -> Result<keyspacepb::KeyspaceMeta> {
        retry_mut!(self, "load_keyspace", |cluster| async {
            cluster.load_keyspace(keyspace, self.timeout).await
        })
    }
}

impl fmt::Debug for RetryClient {
    fn fmt(&self, fmt: &mut fmt::Formatter) -> fmt::Result {
        fmt.debug_struct("pd::RetryClient")
            .field("timeout", &self.timeout)
            .finish()
    }
}

fn region_from_response(
    mut resp: pdpb::GetRegionResponse,
    err: impl FnOnce() -> Error,
) -> Result<RegionWithLeader> {
    let region = resp.region.take().ok_or_else(err)?;
    Ok(RegionWithLeader::new(region, resp.leader.take()))
}

// A node-like thing that can be connected to.
#[async_trait]
trait Reconnect {
    type Cl;
    async fn reconnect(&self, interval_sec: u64) -> Result<()>;
}

#[async_trait]
impl Reconnect for RetryClient<Cluster> {
    type Cl = Cluster;

    async fn reconnect(&self, interval_sec: u64) -> Result<()> {
        let reconnect_begin = Instant::now();
        let mut lock = self.cluster.write().await;
        let (cluster, last_connected) = &mut *lock;
        // If `last_connected + interval_sec` is larger or equal than reconnect_begin,
        // a concurrent reconnect is just succeed when this thread trying to get write lock
        let should_connect = reconnect_begin > *last_connected + Duration::from_secs(interval_sec);
        if should_connect {
            self.connection.reconnect(cluster, self.timeout).await?;
            *last_connected = Instant::now();
        }
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    use std::sync::Mutex;

    use futures::executor;
    use futures::future::ready;

    use super::*;
    use crate::internal_err;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_reconnect() {
        struct MockClient {
            reconnect_count: AtomicUsize,
            cluster: RwLock<((), Instant)>,
        }

        #[async_trait]
        impl Reconnect for MockClient {
            type Cl = ();

            async fn reconnect(&self, _: u64) -> Result<()> {
                self.reconnect_count
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // Not actually unimplemented, we just don't care about the error.
                Err(Error::Unimplemented)
            }
        }

        async fn retry_err(client: Arc<MockClient>) -> Result<()> {
            retry_mut!(client, "test", |_c| ready(Err(internal_err!("whoops"))))
        }

        async fn retry_ok(client: Arc<MockClient>) -> Result<()> {
            retry!(client, "test", |_c| ready(Ok::<_, Error>(())))
        }

        executor::block_on(async {
            let client = Arc::new(MockClient {
                reconnect_count: AtomicUsize::new(0),
                cluster: RwLock::new(((), Instant::now())),
            });

            assert!(retry_err(client.clone()).await.is_err());
            assert_eq!(
                client
                    .reconnect_count
                    .load(std::sync::atomic::Ordering::SeqCst),
                MAX_REQUEST_COUNT
            );

            client
                .reconnect_count
                .store(0, std::sync::atomic::Ordering::SeqCst);
            assert!(retry_ok(client.clone()).await.is_ok());
            assert_eq!(
                client
                    .reconnect_count
                    .load(std::sync::atomic::Ordering::SeqCst),
                0
            );
        })
    }

    #[test]
    fn test_retry() {
        struct MockClient {
            cluster: RwLock<(AtomicUsize, Instant)>,
        }

        #[async_trait]
        impl Reconnect for MockClient {
            type Cl = Mutex<usize>;

            async fn reconnect(&self, _: u64) -> Result<()> {
                Ok(())
            }
        }

        async fn retry_max_err(
            client: Arc<MockClient>,
            max_retries: Arc<AtomicUsize>,
        ) -> Result<()> {
            retry_mut!(client, "test", |c| {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                let max_retries = max_retries.fetch_sub(1, Ordering::SeqCst) - 1;
                if max_retries == 0 {
                    ready(Ok(()))
                } else {
                    ready(Err(internal_err!("whoops")))
                }
            })
        }

        async fn retry_max_ok(
            client: Arc<MockClient>,
            max_retries: Arc<AtomicUsize>,
        ) -> Result<()> {
            retry!(client, "test", |c| {
                c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                let max_retries = max_retries.fetch_sub(1, Ordering::SeqCst) - 1;
                if max_retries == 0 {
                    ready(Ok(()))
                } else {
                    ready(Err(internal_err!("whoops")))
                }
            })
        }

        executor::block_on(async {
            let client = Arc::new(MockClient {
                cluster: RwLock::new((AtomicUsize::new(0), Instant::now())),
            });
            let max_retries = Arc::new(AtomicUsize::new(1000));

            assert!(retry_max_err(client.clone(), max_retries).await.is_err());
            assert_eq!(
                client.cluster.read().await.0.load(Ordering::SeqCst),
                LEADER_CHANGE_RETRY
            );

            let client = Arc::new(MockClient {
                cluster: RwLock::new((AtomicUsize::new(0), Instant::now())),
            });
            let max_retries = Arc::new(AtomicUsize::new(2));

            assert!(retry_max_ok(client.clone(), max_retries).await.is_ok());
            assert_eq!(client.cluster.read().await.0.load(Ordering::SeqCst), 2);
        })
    }

    #[tokio::test]
    async fn timed_retry_reconnects_before_deadline() {
        struct MockClient {
            attempts: AtomicUsize,
            reconnect_count: AtomicUsize,
        }

        #[async_trait]
        impl Reconnect for MockClient {
            type Cl = ();

            async fn reconnect(&self, _: u64) -> Result<()> {
                self.reconnect_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        async fn retry_timed(client: Arc<MockClient>) -> Result<()> {
            retry_core_with_timeout!(client, "test_timed", Duration::from_millis(50), {
                let attempt = client.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    Err(internal_err!("whoops"))
                } else {
                    Ok(())
                }
            })
        }

        let client = Arc::new(MockClient {
            attempts: AtomicUsize::new(0),
            reconnect_count: AtomicUsize::new(0),
        });

        retry_timed(client.clone())
            .await
            .expect("second attempt after reconnect should succeed");
        assert_eq!(client.attempts.load(Ordering::SeqCst), 2);
        assert_eq!(client.reconnect_count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn timed_retry_succeeds_after_slow_first_attempt() {
        // Verifies that when the first attempt truly times out (consumes its
        // per-attempt budget), the retry loop still has enough remaining budget
        // to reconnect and succeed on the second attempt.
        //
        // With the old code (per_attempt = full timeout), the first attempt
        // would consume the entire budget, leaving zero time for retry.
        // With the fix (per_attempt = remaining.min(timeout*2/3)), the first
        // attempt only consumes ~2/3 of the budget.
        struct MockClient {
            attempts: AtomicUsize,
            reconnect_count: AtomicUsize,
        }

        #[async_trait]
        impl Reconnect for MockClient {
            type Cl = ();

            async fn reconnect(&self, _: u64) -> Result<()> {
                self.reconnect_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let total_timeout = Duration::from_millis(300);

        async fn retry_timed(client: Arc<MockClient>, total_timeout: Duration) -> Result<()> {
            let deadline = Instant::now() + total_timeout;
            retry_core_with_timeout!(client, "test_timed", total_timeout, {
                // Mirror the production pattern: cap per-attempt to timeout*2/3
                let remaining = deadline.saturating_duration_since(Instant::now());
                let per_attempt = remaining.min(total_timeout * 2 / 3);
                let attempt = client.attempts.fetch_add(1, Ordering::SeqCst);
                if attempt == 0 {
                    // First attempt: simulate a hung RPC that fully exhausts
                    // its per-attempt budget
                    tokio::time::sleep(per_attempt).await;
                    Err(internal_err!("per-attempt timeout"))
                } else {
                    // Second attempt: PD has recovered, succeed immediately
                    Ok(())
                }
            })
        }

        let client = Arc::new(MockClient {
            attempts: AtomicUsize::new(0),
            reconnect_count: AtomicUsize::new(0),
        });

        let start = Instant::now();
        retry_timed(client.clone(), total_timeout)
            .await
            .expect("should succeed on second attempt after slow first attempt");

        // Must have attempted twice (first slow fail, second success)
        assert_eq!(client.attempts.load(Ordering::SeqCst), 2);
        // Must have reconnected once between attempts
        assert_eq!(client.reconnect_count.load(Ordering::SeqCst), 1);
        // Must complete within the total budget
        assert!(
            start.elapsed() < total_timeout,
            "entire operation should complete within the total timeout budget",
        );
    }

    #[tokio::test]
    async fn timed_retry_allows_slow_successful_first_attempt() {
        // Verifies that a slow-but-healthy PD response (taking more than
        // timeout/3 but less than the per-attempt cap of timeout*2/3)
        // succeeds on the first attempt without unnecessary retries.
        // This guards against over-aggressive per-attempt caps that would
        // reject responses from a slow but functioning PD (e.g. during
        // leader election).
        struct MockClient {
            attempts: AtomicUsize,
            reconnect_count: AtomicUsize,
        }

        #[async_trait]
        impl Reconnect for MockClient {
            type Cl = ();

            async fn reconnect(&self, _: u64) -> Result<()> {
                self.reconnect_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        let total_timeout = Duration::from_millis(300);

        async fn retry_timed(client: Arc<MockClient>, total_timeout: Duration) -> Result<()> {
            let deadline = Instant::now() + total_timeout;
            retry_core_with_timeout!(client, "test_timed", total_timeout, {
                let remaining = deadline.saturating_duration_since(Instant::now());
                let per_attempt = remaining.min(total_timeout * 2 / 3);
                let _attempt = client.attempts.fetch_add(1, Ordering::SeqCst);
                // Simulate slow PD: responds at 150ms (> timeout/3=100ms but
                // within per_attempt cap of timeout*2/3=200ms)
                tokio::time::sleep(Duration::from_millis(150)).await;
                Ok(())
            })
        }

        let client = Arc::new(MockClient {
            attempts: AtomicUsize::new(0),
            reconnect_count: AtomicUsize::new(0),
        });

        retry_timed(client.clone(), total_timeout)
            .await
            .expect("slow but healthy PD response should succeed on first attempt");
        assert_eq!(
            client.attempts.load(Ordering::SeqCst),
            1,
            "should succeed on first attempt without retry",
        );
        assert_eq!(
            client.reconnect_count.load(Ordering::SeqCst),
            0,
            "should not trigger reconnect for slow-but-successful response",
        );
    }

    #[tokio::test]
    async fn timed_retry_uses_total_timeout_budget() {
        struct MockClient {
            reconnect_count: AtomicUsize,
        }

        #[async_trait]
        impl Reconnect for MockClient {
            type Cl = ();

            async fn reconnect(&self, _: u64) -> Result<()> {
                self.reconnect_count.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }
        }

        async fn retry_timed(client: Arc<MockClient>) -> Result<()> {
            retry_core_with_timeout!(client, "test_timed", Duration::from_millis(25), {
                std::future::pending::<Result<()>>().await
            })
        }

        let client = Arc::new(MockClient {
            reconnect_count: AtomicUsize::new(0),
        });

        let start = Instant::now();
        let err = retry_timed(client.clone())
            .await
            .expect_err("timed retry should stop when the total budget expires");

        assert!(
            start.elapsed() < Duration::from_millis(250),
            "total timeout should bound the whole retry operation",
        );
        assert_eq!(
            client.reconnect_count.load(Ordering::SeqCst),
            0,
            "outer timeout should stop the operation before reconnect begins",
        );
        assert!(
            err.to_string().contains("timed out after"),
            "unexpected error: {err}",
        );
    }
}
