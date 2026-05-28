use bytes::Bytes;
use moka::future::Cache as MokaCache;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct CachedResponse {
    pub body: Bytes,
    pub content_type: String,
}

pub struct Cache(MokaCache<String, CachedResponse>);

impl Cache {
    pub fn new(max_capacity: u64, ttl_secs: u64) -> Self {
        let cache = MokaCache::builder()
            .max_capacity(max_capacity)
            .time_to_live(Duration::from_secs(ttl_secs))
            .build();
        Cache(cache)
    }

    pub async fn get_or_fetch<F, Fut>(
        &self,
        key: String,
        fetch: F,
    ) -> anyhow::Result<CachedResponse>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<CachedResponse>>,
    {
        if let Some(cached) = self.0.get(&key).await {
            return Ok(cached);
        }
        let result = fetch().await?;
        self.0.insert(key, result.clone()).await;
        Ok(result)
    }
}
