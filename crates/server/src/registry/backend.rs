//! The object store behind the registry, chosen by `BUILDKIT_CACHE_STORE_URL`.
//!
//! `file:///<abs path>` is the volume: a `LocalFileSystem` rooted there,
//! syncing each file and its directory before a write returns, and removing
//! the empty directories that deletes leave behind. Before it is handed out,
//! a sweep removes every file without a key shape of the layout
//! ([`super::layout`]): the 0.4 tree, and files staged by a process that died
//! mid-write, which listings never show. A 0.4 file left where 0.5 needs a
//! directory would fail every push of that digest or tag.
//!
//! `s3://`, `gs://` and `az://` name a bucket or container, and an optional
//! prefix that belongs to the registry alone. Their options are the
//! variables `BUILDKIT_CACHE_STORE_OPT_<key>`, checked against the
//! provider's keys at start-up ([`StoreConfig::new`]). The raw provider
//! variables (`AWS_*`, `GOOGLE_*`, `AZURE_*`) still reach the builder, as
//! `from_env` would read them, so ambient credentials keep working.

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use object_store::aws::{AmazonS3Builder, AmazonS3ConfigKey};
use object_store::azure::{AzureConfigKey, MicrosoftAzureBuilder};
use object_store::gcp::{GoogleCloudStorageBuilder, GoogleConfigKey};
use object_store::local::LocalFileSystem;
use object_store::prefix::PrefixStore;
use object_store::{BackoffConfig, ClientOptions, ObjectStore, RetryConfig};
use tracing::info;

use super::layout::Key;
use super::store::Parts;

/// The default store: the registry's volume.
pub const DEFAULT_URL: &str = "file:///var/lib/buildkit-cache";

/// The prefix of an option variable: `BUILDKIT_CACHE_STORE_OPT_<key>`.
pub const OPTION_PREFIX: &str = "BUILDKIT_CACHE_STORE_OPT_";

/// A cloud store provider, one URL scheme each.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Provider {
    S3,
    Gcs,
    Azure,
}

impl Provider {
    fn scheme(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::Gcs => "gs",
            Self::Azure => "az",
        }
    }
}

/// Where a store URL points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreUrl {
    /// A directory on a volume.
    File(PathBuf),
    /// A bucket (an Azure container), and the prefix under it, if any.
    Bucket {
        provider: Provider,
        bucket: String,
        prefix: Option<String>,
    },
}

impl std::fmt::Display for StoreUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::File(p) => write!(f, "file://{}", p.display()),
            Self::Bucket {
                provider,
                bucket,
                prefix,
            } => {
                write!(f, "{}://{bucket}", provider.scheme())?;
                match prefix {
                    Some(p) => write!(f, "/{p}"),
                    None => Ok(()),
                }
            }
        }
    }
}

impl StoreUrl {
    /// Parses `s`: `file://` with an absolute path, or `s3://`, `gs://` or
    /// `az://` with a bucket and an optional prefix. Every other scheme
    /// `object_store` knows, such as `s3a` or `abfss`, is refused.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let url = url::Url::parse(s).with_context(|| format!("store URL `{s}` does not parse"))?;
        if url.query().is_some() || url.fragment().is_some() {
            bail!("store URL `{s}` has a query or fragment");
        }
        let provider = match url.scheme() {
            "file" => {
                if url.host_str().is_some_and(|h| !h.is_empty()) {
                    bail!("store URL `{s}` names a host; use file:///<absolute path>");
                }
                let path = url
                    .to_file_path()
                    .map_err(|()| anyhow::anyhow!("store URL `{s}` needs an absolute path"))?;
                return Ok(Self::File(path));
            }
            "s3" => Provider::S3,
            "gs" => Provider::Gcs,
            "az" => Provider::Azure,
            other => bail!(
                "store URL scheme `{other}` is not served; use file:///<absolute path>, s3://, gs:// or az://<bucket>[/<prefix>]"
            ),
        };
        if !url.username().is_empty() || url.password().is_some() || url.port().is_some() {
            bail!("store URL `{s}` holds a user or a port; set them as options");
        }
        let bucket = url
            .host_str()
            .filter(|h| !h.is_empty())
            .with_context(|| format!("store URL `{s}` names no bucket"))?
            .to_owned();
        let raw = url.path().trim_matches('/');
        let prefix = if raw.is_empty() {
            None
        } else {
            let p = object_store::path::Path::parse(raw)
                .with_context(|| format!("store URL `{s}` has a bad prefix"))?;
            Some(p.to_string())
        };
        Ok(Self::Bucket {
            provider,
            bucket,
            prefix,
        })
    }

    /// How the store takes upload parts: the volume takes each chunk as it
    /// comes, a bucket takes parts of a minimum size.
    #[must_use]
    pub fn parts(&self) -> Parts {
        match self {
            Self::File(_) => Parts::Direct,
            Self::Bucket { .. } => Parts::Buffered,
        }
    }
}

/// Settings for one provider: raw provider variables, then options. Values
/// hold credentials, so `Debug` shows the keys only.
#[derive(Clone)]
pub(super) struct Settings<K> {
    /// Provider variables (`AWS_*` and so on) that parse as a key.
    pub raw: Vec<(K, String)>,
    /// `BUILDKIT_CACHE_STORE_OPT_*`, in name order.
    pub options: Vec<(K, String)>,
}

impl<K: std::fmt::Debug> std::fmt::Debug for Settings<K> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys = |v: &[(K, String)]| v.iter().map(|(k, _)| format!("{k:?}")).collect::<Vec<_>>();
        f.debug_struct("Settings")
            .field("raw", &keys(&self.raw))
            .field("options", &keys(&self.options))
            .finish()
    }
}

impl<K> Settings<K>
where
    K: FromStr + PartialEq + Copy,
{
    /// Reads `env`. Raw variables that do not parse are skipped, as
    /// `from_env` skips them; an option that does not parse, that sets
    /// `owned` (the URL's bucket or container), or that names a key another
    /// option names too fails, naming the option.
    fn parse(
        env: &[(String, String)],
        raw_prefix: &str,
        owned: K,
        scheme: &str,
    ) -> anyhow::Result<Self> {
        let raw = env
            .iter()
            .filter(|(k, _)| k.starts_with(raw_prefix))
            .filter_map(|(k, v)| {
                let key = k.to_ascii_lowercase().parse().ok()?;
                Some((key, v.clone()))
            })
            .collect();
        let mut named: Vec<(String, &String)> = env
            .iter()
            .filter_map(|(k, v)| Some((k.strip_prefix(OPTION_PREFIX)?.to_ascii_lowercase(), v)))
            .collect();
        named.sort();
        let mut options: Vec<(K, String)> = Vec::new();
        let mut seen: Vec<(K, String)> = Vec::new();
        for (name, value) in named {
            let Ok(key) = name.parse::<K>() else {
                bail!("store option `{name}` ({OPTION_PREFIX}{name}) is not a key of {scheme}://");
            };
            if key == owned {
                bail!("store option `{name}` sets the bucket, which the store URL names");
            }
            if let Some((_, first)) = seen.iter().find(|(k, _)| *k == key) {
                bail!("store options `{first}` and `{name}` name the same key; keep one");
            }
            seen.push((key, name));
            options.push((key, value.clone()));
        }
        Ok(Self { raw, options })
    }
}

/// The checked settings of a cloud store.
#[derive(Clone, Debug)]
pub(super) enum Cloud {
    S3(Settings<AmazonS3ConfigKey>),
    /// With `SERVICE_ACCOUNT`, read as `from_env` reads it.
    Gcs(Settings<GoogleConfigKey>, Option<String>),
    /// With `IDENTITY_ENDPOINT`, the managed identity endpoint.
    Azure(Settings<AzureConfigKey>, Option<String>),
}

/// A store URL with its options, checked.
#[derive(Clone, Debug)]
pub struct StoreConfig {
    pub url: StoreUrl,
    pub(super) cloud: Option<Cloud>,
}

/// Client defaults for the cloud stores, set before the options: no
/// whole-request timeout, which would cut every long blob GET, 60 s between
/// body chunks, 5 s to connect.
fn client_options() -> ClientOptions {
    ClientOptions::new()
        .with_timeout_disabled()
        .with_read_timeout(Duration::from_mins(1))
        .with_connect_timeout(Duration::from_secs(5))
}

/// At most 3 retries within 15 s, so a request does not hang for minutes
/// in an outage.
fn retry_config() -> RetryConfig {
    RetryConfig {
        backoff: BackoffConfig::default(),
        max_retries: 3,
        retry_timeout: Duration::from_secs(15),
    }
}

impl StoreConfig {
    /// Checks the options for `url` in `env`, the process environment or a
    /// rendered pod's. Fails, naming the variable, on any option that is not
    /// a key of the provider, on two options for one key, on an option that
    /// sets the bucket, and on any option for a `file` store.
    pub fn new<I, K, V>(url: StoreUrl, env: I) -> anyhow::Result<Self>
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let env: Vec<(String, String)> =
            env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        let get = |name: &str| env.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
        let cloud = match &url {
            StoreUrl::File(_) => {
                if let Some((k, _)) = env.iter().find(|(k, _)| k.starts_with(OPTION_PREFIX)) {
                    bail!("{k} is set, but a file:// store takes no options");
                }
                None
            }
            StoreUrl::Bucket { provider, .. } => {
                let s = provider.scheme();
                Some(match provider {
                    Provider::S3 => {
                        Cloud::S3(Settings::parse(&env, "AWS_", AmazonS3ConfigKey::Bucket, s)?)
                    }
                    Provider::Gcs => Cloud::Gcs(
                        Settings::parse(&env, "GOOGLE_", GoogleConfigKey::Bucket, s)?,
                        get("SERVICE_ACCOUNT"),
                    ),
                    Provider::Azure => Cloud::Azure(
                        Settings::parse(&env, "AZURE_", AzureConfigKey::ContainerName, s)?,
                        get("IDENTITY_ENDPOINT"),
                    ),
                })
            }
        };
        Ok(Self { url, cloud })
    }

    /// Opens the store. On the volume this creates the directory if needed
    /// and sweeps it; blocking, and it must finish before the registry
    /// serves. For a bucket it only builds the client: an unreachable store
    /// shows at the first listing.
    pub fn open(&self) -> anyhow::Result<Arc<dyn ObjectStore>> {
        let (root, base) = match &self.url {
            StoreUrl::File(root) => return open_volume(root),
            StoreUrl::Bucket {
                provider,
                bucket,
                prefix,
            } => (prefix, format!("{}://{bucket}", provider.scheme())),
        };
        // reqwest runs on rustls without a provider of its own.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let store: Arc<dyn ObjectStore> = match &self.cloud {
            Some(Cloud::S3(s)) => {
                let mut b = AmazonS3Builder::new()
                    .with_client_options(client_options())
                    .with_retry(retry_config());
                for (k, v) in s.raw.iter().chain(&s.options) {
                    b = b.with_config(*k, v);
                }
                Arc::new(b.with_url(&base).build()?)
            }
            Some(Cloud::Gcs(s, account)) => {
                let mut b = GoogleCloudStorageBuilder::new()
                    .with_client_options(client_options())
                    .with_retry(retry_config());
                if let Some(a) = account {
                    b = b.with_service_account_path(a);
                }
                for (k, v) in s.raw.iter().chain(&s.options) {
                    b = b.with_config(*k, v);
                }
                Arc::new(b.with_url(&base).build()?)
            }
            Some(Cloud::Azure(s, msi)) => {
                let mut b = MicrosoftAzureBuilder::new()
                    .with_client_options(client_options())
                    .with_retry(retry_config());
                for (k, v) in &s.raw {
                    b = b.with_config(*k, v);
                }
                if let Some(e) = msi {
                    b = b.with_msi_endpoint(e);
                }
                for (k, v) in &s.options {
                    b = b.with_config(*k, v);
                }
                Arc::new(b.with_url(&base).build()?)
            }
            None => bail!("store {} has no settings", self.url),
        };
        let names: Vec<&str> = self.option_names();
        info!(store_url = %self.url, options = ?names, "store client built");
        Ok(match root {
            Some(p) => Arc::new(PrefixStore::new(store, p.as_str())),
            None => store,
        })
    }

    /// The option keys set, never their values.
    fn option_names(&self) -> Vec<&str> {
        fn names<K: AsRef<str>>(s: &Settings<K>) -> Vec<&str> {
            s.options.iter().map(|(k, _)| k.as_ref()).collect()
        }
        match &self.cloud {
            Some(Cloud::S3(s)) => names(s),
            Some(Cloud::Gcs(s, _)) => names(s),
            Some(Cloud::Azure(s, _)) => names(s),
            None => Vec::new(),
        }
    }
}

/// Opens the volume at `root`: creates it if needed, sweeps it, and roots a
/// `LocalFileSystem` there.
pub(super) fn open_volume(root: &Path) -> anyhow::Result<Arc<dyn ObjectStore>> {
    std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
    let removed = sweep(root).with_context(|| format!("sweeping {}", root.display()))?;
    if removed > 0 {
        info!(root = %root.display(), removed, "removed files outside the store layout");
    }
    let fs = LocalFileSystem::new_with_prefix(root)
        .with_context(|| format!("opening {}", root.display()))?
        .with_fsync(true)
        .with_automatic_cleanup(true);
    Ok(Arc::new(fs))
}

/// Removes every file under `root` that is not a key of the layout, then
/// every empty directory below `root`. Returns how many files it removed.
fn sweep(root: &Path) -> std::io::Result<u64> {
    fn walk(root: &Path, dir: &Path, removed: &mut u64) -> std::io::Result<()> {
        for e in std::fs::read_dir(dir)? {
            let e = e?;
            let path = e.path();
            if e.file_type()?.is_dir() {
                walk(root, &path, removed)?;
                if std::fs::read_dir(&path)?.next().is_none() {
                    std::fs::remove_dir(&path)?;
                }
                continue;
            }
            let key = path
                .strip_prefix(root)
                .ok()
                .and_then(Path::to_str)
                .map(|s| s.replace(std::path::MAIN_SEPARATOR, "/"));
            if key.as_deref().and_then(Key::parse).is_none() {
                std::fs::remove_file(&path)?;
                *removed += 1;
            }
        }
        Ok(())
    }
    let mut removed = 0;
    walk(root, root, &mut removed)?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn bucket(provider: Provider, bucket: &str, prefix: Option<&str>) -> StoreUrl {
        StoreUrl::Bucket {
            provider,
            bucket: bucket.to_owned(),
            prefix: prefix.map(str::to_owned),
        }
    }

    #[test]
    fn store_urls_parse() {
        assert_eq!(
            StoreUrl::parse(DEFAULT_URL).unwrap(),
            StoreUrl::File("/var/lib/buildkit-cache".into())
        );
        assert_eq!(
            StoreUrl::parse("file:///data/a%20b").unwrap(),
            StoreUrl::File("/data/a b".into())
        );
        for (s, want) in [
            ("s3://b", bucket(Provider::S3, "b", None)),
            ("s3://b/", bucket(Provider::S3, "b", None)),
            (
                "s3://b/cache/x/",
                bucket(Provider::S3, "b", Some("cache/x")),
            ),
            ("gs://b/p", bucket(Provider::Gcs, "b", Some("p"))),
            ("az://c/p", bucket(Provider::Azure, "c", Some("p"))),
        ] {
            let got = StoreUrl::parse(s).unwrap();
            assert_eq!(got, want, "{s}");
            assert_eq!(got.parts(), Parts::Buffered);
        }
        assert_eq!(
            StoreUrl::parse("s3://b/cache/x/").unwrap().to_string(),
            "s3://b/cache/x"
        );
        for bad in [
            "/var/lib/buildkit-cache",
            "file://host/data",
            "file:///data?x=1",
            "s3a://bucket",
            "abfss://c@a.dfs.core.windows.net/p",
            "http://example.invalid/x",
            "s3://",
            "s3://user@bucket/p",
            "s3://bucket:9000/p",
            "s3://bucket/p?region=x",
            "gs://bucket/a//b",
        ] {
            assert!(StoreUrl::parse(bad).is_err(), "{bad}");
        }
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    fn s3_options(c: &StoreConfig) -> Vec<(AmazonS3ConfigKey, String)> {
        match &c.cloud {
            Some(Cloud::S3(s)) => s.options.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn options_parse_into_the_provider_keys() {
        let url = StoreUrl::parse("s3://b/p").unwrap();
        let c = StoreConfig::new(
            url.clone(),
            env(&[
                ("BUILDKIT_CACHE_STORE_OPT_region", "eu-west-1"),
                ("BUILDKIT_CACHE_STORE_OPT_ALLOW_HTTP", "true"),
                ("AWS_REGION", "us-east-1"),
                ("AWS_STS_REGIONAL_ENDPOINTS", "regional"),
                ("AWS_NOT_A_KEY", "x"),
                ("HOME", "/root"),
            ]),
        )
        .unwrap();
        assert_eq!(
            s3_options(&c),
            vec![
                (
                    AmazonS3ConfigKey::Client(object_store::ClientConfigKey::AllowHttp),
                    "true".to_owned()
                ),
                (AmazonS3ConfigKey::Region, "eu-west-1".to_owned()),
            ]
        );
        // Raw provider variables pass, the unknown ones without a word.
        match &c.cloud {
            Some(Cloud::S3(s)) => {
                assert_eq!(
                    s.raw,
                    vec![(AmazonS3ConfigKey::Region, "us-east-1".to_owned())]
                );
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(c.option_names(), ["allow_http", "aws_region"]);
        // Values hold credentials: never in a debug print.
        let shown = format!("{c:?}");
        assert!(
            !shown.contains("eu-west-1") && !shown.contains("us-east-1"),
            "{shown}"
        );
        // Builds without a network call.
        c.open().unwrap();
    }

    #[test]
    fn a_misspelt_option_fails_naming_it() {
        let url = StoreUrl::parse("s3://b").unwrap();
        // Set in the chart's options or through the Secret: one key space.
        for name in [
            "BUILDKIT_CACHE_STORE_OPT_regoin",
            "BUILDKIT_CACHE_STORE_OPT_REGOIN",
        ] {
            let e = StoreConfig::new(url.clone(), env(&[(name, "x")]))
                .unwrap_err()
                .to_string();
            assert!(e.contains("`regoin`"), "{e}");
            assert!(!e.contains("`x`"), "{e}");
        }
        let e = StoreConfig::new(
            url.clone(),
            env(&[
                ("BUILDKIT_CACHE_STORE_OPT_region", "a"),
                ("BUILDKIT_CACHE_STORE_OPT_aws_region", "b"),
            ]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("aws_region") && e.contains("region"), "{e}");
        for owned in ["bucket", "aws_bucket_name"] {
            let name = format!("{OPTION_PREFIX}{owned}");
            let e = StoreConfig::new(url.clone(), env(&[(&name, "other")]))
                .unwrap_err()
                .to_string();
            assert!(e.contains(owned) && e.contains("bucket"), "{e}");
        }
        let gs = StoreUrl::parse("gs://b").unwrap();
        assert!(
            StoreConfig::new(gs.clone(), env(&[("BUILDKIT_CACHE_STORE_OPT_region", "x")])).is_err()
        );
        let c = StoreConfig::new(
            gs,
            env(&[("BUILDKIT_CACHE_STORE_OPT_service_account_key", "{}")]),
        );
        assert!(c.is_ok());
        let az = StoreUrl::parse("az://c").unwrap();
        let e = StoreConfig::new(az, env(&[("BUILDKIT_CACHE_STORE_OPT_container_name", "x")]))
            .unwrap_err()
            .to_string();
        assert!(e.contains("container_name"), "{e}");
        let file = StoreUrl::parse(DEFAULT_URL).unwrap();
        let e = StoreConfig::new(file, env(&[("BUILDKIT_CACHE_STORE_OPT_region", "x")]))
            .unwrap_err()
            .to_string();
        assert!(e.contains("BUILDKIT_CACHE_STORE_OPT_region"), "{e}");
    }

    #[test]
    fn each_provider_builds_without_a_network_call() {
        for (url, opts) in [
            ("gs://b/p", vec![]),
            (
                "az://c/p",
                vec![
                    ("BUILDKIT_CACHE_STORE_OPT_account_name", "acct"),
                    ("BUILDKIT_CACHE_STORE_OPT_use_emulator", "true"),
                ],
            ),
        ] {
            let c = StoreConfig::new(StoreUrl::parse(url).unwrap(), env(&opts)).unwrap();
            c.open().unwrap();
        }
    }
}
