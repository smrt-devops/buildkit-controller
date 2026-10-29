//! Flags and environment of the `cache` role.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::config::LogFormat;

/// `buildkit-controller cache [flags]`.
#[derive(Clone, Debug, clap::Parser)]
#[command(
    name = "buildkit-controller cache",
    about = "Run the built-in cache registry"
)]
pub struct CacheArgs {
    /// Registry listener (mTLS).
    #[arg(long, env = "BUILDKIT_CACHE_LISTEN", default_value = "0.0.0.0:5000")]
    pub listen: SocketAddr,
    /// Plaintext `/metrics`, `/healthz` and `/readyz`.
    #[arg(
        long,
        env = "BUILDKIT_CACHE_OPS_LISTEN",
        default_value = "0.0.0.0:8080"
    )]
    pub ops_listen: SocketAddr,
    /// The store: one directory on one volume.
    #[arg(
        long,
        env = "BUILDKIT_CACHE_DATA_DIR",
        default_value = "/var/lib/buildkit-cache"
    )]
    pub data_dir: PathBuf,
    /// Holds `tls.crt` and `tls.key` (the serving cert, from the internal CA) and
    /// `ca.crt` (the client CA, the only anchor for the workers' registry client
    /// certs). Re-read every minute.
    #[arg(
        long,
        env = "BUILDKIT_CACHE_TLS_DIR",
        default_value = "/etc/buildkit-cache/tls"
    )]
    pub tls_dir: PathBuf,
    /// Namespaces whose pools and worker pods the role watches to authorize
    /// workers, comma-separated; empty = all. The controller's `watchNamespaces`.
    #[arg(long, env = "BUILDKIT_CACHE_WATCH_NAMESPACES", value_delimiter = ',')]
    pub watch_namespaces: Vec<String>,
    /// YAML list of static grants (`namespace`, `pod`, `repository`, `push`), for
    /// tests: with it the role grants from the file instead of watching pods,
    /// and every pool gets `--pool-max-size`.
    #[arg(long, env = "BUILDKIT_CACHE_GRANTS_FILE")]
    pub grants_file: Option<PathBuf>,
    /// Budget of the whole volume, e.g. `50Gi`.
    #[arg(long, env = "BUILDKIT_CACHE_MAX_SIZE", default_value = "50Gi", value_parser = parse_size)]
    pub max_size: u64,
    /// Budget of a pool without `cache.maxSize` (repositories under `<namespace>/<pool>/`).
    #[arg(long, env = "BUILDKIT_CACHE_POOL_MAX_SIZE", default_value = "10Gi", value_parser = parse_size)]
    pub pool_max_size: u64,
    /// Largest blob accepted.
    #[arg(long, env = "BUILDKIT_CACHE_MAX_BLOB_SIZE", default_value = "5Gi", value_parser = parse_size)]
    pub max_blob_size: u64,
    /// Bytes all uploads in progress may hold together; one worker may hold
    /// a quarter. The volume needs room for `--max-size` plus this.
    #[arg(long, env = "BUILDKIT_CACHE_MAX_UPLOAD_SIZE", default_value = "10Gi", value_parser = parse_size)]
    pub max_upload_size: u64,
    /// Time between garbage collections, e.g. `10m`.
    #[arg(long, env = "BUILDKIT_CACHE_GC_INTERVAL", default_value = "10m", value_parser = parse_duration)]
    pub gc_interval: Duration,
    /// Log filter, e.g. `info` or `buildkit_controller=debug`.
    #[arg(long, env = "LOG_LEVEL", default_value = "info")]
    pub log_level: String,
    /// Log encoding.
    #[arg(long, env = "LOG_FORMAT", value_enum, default_value = "json")]
    pub log_format: LogFormat,
}

/// Bytes, with an optional `Ki`, `Mi`, `Gi` or `Ti` suffix.
pub fn parse_size(s: &str) -> Result<u64, String> {
    let (num, mult) = [
        ("Ki", 1u64 << 10),
        ("Mi", 1 << 20),
        ("Gi", 1 << 30),
        ("Ti", 1 << 40),
    ]
    .iter()
    .find_map(|(suf, m)| s.strip_suffix(suf).map(|n| (n, *m)))
    .unwrap_or((s, 1));
    num.parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .ok_or_else(|| format!("`{s}` is not a size such as 512Mi or 10Gi"))
}

/// Whole seconds, minutes or hours: `30s`, `10m`, `1h`.
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let bad = || format!("`{s}` is not a duration such as 30s, 10m or 1h");
    let (num, mult) = [("s", 1u64), ("m", 60), ("h", 3600)]
        .iter()
        .find_map(|(suf, m)| s.strip_suffix(suf).map(|n| (n, *m)))
        .ok_or_else(bad)?;
    let secs = num
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(mult))
        .filter(|n| *n > 0)
        .ok_or_else(bad)?;
    Ok(Duration::from_secs(secs))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use clap::Parser;

    #[test]
    fn sizes_and_durations_parse() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("2Ki").unwrap(), 2048);
        assert_eq!(parse_size("10Gi").unwrap(), 10 << 30);
        assert!(parse_size("10G").is_err());
        assert!(parse_size("99999999999Ti").is_err());
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_mins(10));
        assert!(parse_duration("0s").is_err());
        assert!(parse_duration("10").is_err());
    }

    #[test]
    fn defaults_parse() {
        let a = CacheArgs::try_parse_from(["cache"]).unwrap();
        assert_eq!(a.listen.port(), 5000);
        assert_eq!(a.max_blob_size, 5 << 30);
        assert!(a.grants_file.is_none());
    }
}
