use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub port: u16,
    pub proxy_base: String,
    pub cache_max_capacity: u64,
    pub upstream_timeout_secs: u64,
    pub log_level: String,
    pub transmux_ts: bool,
}

impl Config {
    pub fn from_env() -> Self {
        let port = env::var("PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3000u16);

        let proxy_base = env::var("PROXY_BASE").unwrap_or_default();

        let cache_max_capacity = env::var("CACHE_MAX_CAPACITY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500u64);

        let upstream_timeout_secs = env::var("UPSTREAM_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(15u64);

        let log_level = env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string());

        let transmux_ts = env::var("TRANSMUX_TS")
            .map(|v| v == "true" || v == "1")
            .unwrap_or(false);

        Config {
            port,
            proxy_base,
            cache_max_capacity,
            upstream_timeout_secs,
            log_level,
            transmux_ts,
        }
    }
}
