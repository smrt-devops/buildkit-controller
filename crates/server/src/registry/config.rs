//! Flags and environment of the `registry` role.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use super::backend::{DEFAULT_URL, StoreUrl};
use crate::config::LogFormat;

/// `buildkit-controller registry [flags]`.
#[derive(Clone, Debug, clap::Parser)]
#[command(
    name = "buildkit-controller registry",
    version,
    about = "Run the built-in cache registry"
)]
pub struct RegistryArgs {
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
    /// The store, as a URL: `file:///<absolute path>` for a directory on a
    /// volume, or `s3://`, `gs://` or `az://<bucket>[/<prefix>]`. Options are
    /// the variables `BUILDKIT_CACHE_STORE_OPT_<key>`, with the provider's keys.
    #[arg(long, env = "BUILDKIT_CACHE_STORE_URL", default_value = DEFAULT_URL, value_parser = parse_store_url)]
    pub store_url: StoreUrl,
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
    /// Budget of the whole store, e.g. `40Gi`.
    #[arg(long, env = "BUILDKIT_CACHE_MAX_SIZE", default_value = "40Gi", value_parser = parse_size)]
    pub max_size: u64,
    /// Budget of a pool without `cache.maxSize` (repositories under `<namespace>/<pool>/`).
    #[arg(long, env = "BUILDKIT_CACHE_POOL_MAX_SIZE", default_value = "10Gi", value_parser = parse_size)]
    pub pool_max_size: u64,
    /// Largest blob accepted.
    #[arg(long, env = "BUILDKIT_CACHE_MAX_BLOB_SIZE", default_value = "5Gi", value_parser = parse_size)]
    pub max_blob_size: u64,
    /// Bytes all uploads in progress may hold together; one worker may hold
    /// a quarter. The store needs room for `--max-size` plus this.
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

/// The variable that named the data directory before the store URL. Clap
/// ignores unknown variables, so a stale setting would fall back to the
/// default store without a word; the role refuses to start instead.
pub const OLD_DATA_DIR: &str = "BUILDKIT_CACHE_DATA_DIR";

/// Fails when [`OLD_DATA_DIR`] is set, with `value` its value.
pub fn refuse_data_dir(value: Option<std::ffi::OsString>) -> anyhow::Result<()> {
    match value {
        Some(v) => anyhow::bail!(
            "{OLD_DATA_DIR} is no longer read; unset it and set BUILDKIT_CACHE_STORE_URL=file://{} instead",
            std::path::Path::new(&v).display()
        ),
        None => Ok(()),
    }
}

fn parse_store_url(s: &str) -> Result<StoreUrl, String> {
    StoreUrl::parse(s).map_err(|e| format!("{e:#}"))
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
        let a = RegistryArgs::try_parse_from(["registry"]).unwrap();
        assert_eq!(a.listen.port(), 5000);
        assert_eq!(a.max_blob_size, 5 << 30);
        assert_eq!(a.max_size, 40 << 30);
        assert_eq!(
            a.store_url,
            StoreUrl::File("/var/lib/buildkit-cache".into())
        );
        assert!(a.grants_file.is_none());
    }

    #[test]
    fn the_old_data_dir_variable_is_refused() {
        assert!(refuse_data_dir(None).is_ok());
        let e = refuse_data_dir(Some("/data".into()))
            .unwrap_err()
            .to_string();
        assert!(
            e.contains(OLD_DATA_DIR) && e.contains("BUILDKIT_CACHE_STORE_URL=file:///data"),
            "{e}"
        );
        assert!(RegistryArgs::try_parse_from(["registry", "--data-dir=/data"]).is_err());
        assert!(RegistryArgs::try_parse_from(["registry", "--store-url=s3a://bucket"]).is_err());
    }

    /// The `env` of the `registry` container in a rendered chart, with the
    /// `envFrom` prefix of its Secret.
    fn rendered_env(golden: &str) -> (Vec<(String, String)>, String) {
        let docs: Vec<serde_json::Value> = serde_saphyr::from_multiple(golden).unwrap();
        let c = docs
            .iter()
            .filter(|d| d["kind"] == "StatefulSet")
            .flat_map(|d| {
                d["spec"]["template"]["spec"]["containers"]
                    .as_array()
                    .unwrap()
            })
            .find(|c| c["name"] == "registry")
            .unwrap();
        let env = c["env"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| {
                let value = e["value"].as_str()?;
                Some((e["name"].as_str().unwrap().to_owned(), value.to_owned()))
            })
            .collect();
        let prefix = c["envFrom"][0]["prefix"].as_str().unwrap().to_owned();
        (env, prefix)
    }

    /// Command-line arguments for the variables the role reads through clap.
    /// A variable the role reads nowhere fails the test.
    fn args_from_env(env: &[(String, String)]) -> Vec<String> {
        use clap::CommandFactory;
        let cmd = RegistryArgs::command();
        let mut args = vec!["registry".to_owned()];
        for (k, v) in env {
            if k.starts_with(super::super::backend::OPTION_PREFIX) {
                continue;
            }
            let arg = cmd
                .get_arguments()
                .find(|a| a.get_env().is_some_and(|e| e == k.as_str()))
                .unwrap_or_else(|| panic!("the chart sets {k}, which the role does not read"));
            args.push(format!("--{}={v}", arg.get_long().unwrap()));
        }
        args
    }

    /// The chart's rendered registry env, fed to the role, gives back the
    /// store URL and its options: the env names and the option prefix of the
    /// chart and the role cannot drift apart.
    #[test]
    fn the_rendered_chart_env_reaches_the_role() {
        use super::super::backend::{Cloud, Provider, StoreConfig};
        use object_store::aws::AmazonS3ConfigKey;
        const GOLDEN: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../xtask/fixtures/helm-test/golden/cache-store.yaml"
        ));
        let (mut env, prefix) = rendered_env(GOLDEN);
        let a = RegistryArgs::try_parse_from(args_from_env(&env)).unwrap();
        assert_eq!(
            a.store_url,
            StoreUrl::Bucket {
                provider: Provider::S3,
                bucket: "example-build-cache".into(),
                prefix: Some("buildkit".into()),
            }
        );
        assert_eq!(a.store_url.parts(), super::super::store::Parts::Buffered);
        // A key of the Secret arrives through envFrom with the prefix.
        env.push((format!("{prefix}access_key_id"), "AKIDEXAMPLE".into()));
        let c = StoreConfig::new(a.store_url.clone(), env.clone()).unwrap();
        let Some(Cloud::S3(s)) = &c.cloud else {
            panic!("{:?}", c.cloud)
        };
        assert_eq!(
            s.options,
            vec![
                (AmazonS3ConfigKey::AccessKeyId, "AKIDEXAMPLE".to_owned()),
                (
                    AmazonS3ConfigKey::Endpoint,
                    "https://s3.example.com".to_owned()
                ),
                (AmazonS3ConfigKey::Region, "eu-west-1".to_owned()),
            ]
        );
        // A misspelt Secret key stops the role, naming the key.
        env.push((format!("{prefix}regoin"), "eu-west-1".into()));
        let e = StoreConfig::new(a.store_url, env).unwrap_err().to_string();
        assert!(e.contains("`regoin`"), "{e}");
    }

    #[test]
    fn version_prints_the_crate_version() {
        let e = RegistryArgs::try_parse_from(["registry", "--version"]).unwrap_err();
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(e.to_string().contains(env!("CARGO_PKG_VERSION")));
    }
}
