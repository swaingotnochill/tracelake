use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

/// Default longest a single read request may run.
pub const DEFAULT_QUERY_TIMEOUT: Duration = Duration::from_secs(30);

/// Default number of concurrent read requests allowed per tenant.
pub const DEFAULT_TENANT_CONCURRENCY: usize = 16;

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub query_timeout: Duration,
    pub max_concurrent_per_tenant: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            query_timeout: DEFAULT_QUERY_TIMEOUT,
            max_concurrent_per_tenant: DEFAULT_TENANT_CONCURRENCY,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitError {
    /// The request exceeded the query timeout.
    Timeout,
    /// The tenant already has the maximum number of reads in flight.
    Busy,
}

/// Per-tenant concurrency and a timeout applied to read requests. One process
/// owns its own limits; a second process has its own caps.
pub struct TenantLimiter {
    limits: Limits,
    tenants: Mutex<HashMap<String, Arc<Semaphore>>>,
}

impl TenantLimiter {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            tenants: Mutex::new(HashMap::new()),
        }
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Runs one read request under the tenant's concurrency cap and the query
    /// timeout. The permit is released when this returns.
    pub async fn run<F: Future>(&self, tenant: &str, future: F) -> Result<F::Output, LimitError> {
        let semaphore = self.semaphore(tenant);
        let permit = semaphore
            .try_acquire_owned()
            .map_err(|_| LimitError::Busy)?;

        let result = tokio::time::timeout(self.limits.query_timeout, future).await;
        drop(permit);

        result.map_err(|_| LimitError::Timeout)
    }

    fn semaphore(&self, tenant: &str) -> Arc<Semaphore> {
        let mut tenants = match self.tenants.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };

        tenants
            .entry(tenant.to_string())
            .or_insert_with(|| Arc::new(Semaphore::new(self.limits.max_concurrent_per_tenant)))
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    fn limiter(timeout: Duration, permits: usize) -> Arc<TenantLimiter> {
        Arc::new(TenantLimiter::new(Limits {
            query_timeout: timeout,
            max_concurrent_per_tenant: permits,
        }))
    }

    #[tokio::test]
    async fn a_fast_request_succeeds() {
        let limiter = limiter(Duration::from_secs(1), 1);

        let result = limiter.run("tenant-a", async { 7 }).await;

        assert_eq!(result, Ok(7));
    }

    #[tokio::test]
    async fn a_slow_request_times_out() {
        let limiter = limiter(Duration::from_millis(5), 1);

        let result = limiter
            .run("tenant-a", async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                1
            })
            .await;

        assert_eq!(result, Err(LimitError::Timeout));
    }

    #[tokio::test]
    async fn the_cap_rejects_the_next_request_until_one_finishes() {
        let limiter = limiter(Duration::from_secs(5), 1);
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();

        let held = tokio::spawn({
            let limiter = Arc::clone(&limiter);
            async move {
                limiter
                    .run("tenant-a", async {
                        started_tx.send(()).ok();
                        release_rx.await.ok();
                        1
                    })
                    .await
            }
        });

        started_rx.await.expect("first request started");

        let rejected = limiter.run("tenant-a", async { 2 }).await;
        assert_eq!(rejected, Err(LimitError::Busy));

        let other_tenant = limiter.run("tenant-b", async { 3 }).await;
        assert_eq!(other_tenant, Ok(3), "caps are per tenant");

        release_tx.send(()).ok();
        assert_eq!(held.await.expect("held request joins"), Ok(1));

        let accepted = limiter.run("tenant-a", async { 4 }).await;
        assert_eq!(accepted, Ok(4), "the permit is released");
    }

    #[tokio::test]
    async fn a_zero_cap_rejects_every_request() {
        let limiter = limiter(Duration::from_secs(1), 0);

        let result = limiter.run("tenant-a", async { 1 }).await;

        assert_eq!(result, Err(LimitError::Busy));
    }

    #[test]
    fn default_limits_are_not_unbounded() {
        let limits = Limits::default();

        assert_eq!(limits.query_timeout, Duration::from_secs(30));
        assert_eq!(limits.max_concurrent_per_tenant, 16);
    }
}
