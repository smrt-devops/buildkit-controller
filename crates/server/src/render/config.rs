//! The worker's `buildkitd.toml`: the pool's config with the built-in cache
//! registry added, so buildkitd reaches it over mTLS with the worker's own
//! key pair.

use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TomlError, Value};

use super::hash::sha256_hex;

/// The pool's `buildkitdConfig` with a `[registry."<cache_host>"]` entry that
/// trusts the internal CA and presents the worker's registry client certificate
/// (`client.crt`, from the client CA, which no buildkitd trusts). Without a
/// cache host, or for a config [`with_registry`] refuses, the config is
/// returned unchanged: `validate_template` reports a refused one, so no pod
/// is rendered from it.
#[must_use]
pub fn buildkitd_config(cfg: &str, cache_host: Option<&str>) -> String {
    cache_host
        .and_then(|h| with_registry(cfg, h).ok())
        .unwrap_or_else(|| cfg.to_owned())
}

/// `cfg` with the registry entry of `host`, replacing one the pool set, or why
/// `cfg` cannot take it: it is not TOML, or `registry` or the host's entry is
/// not a table. The rest keeps its formatting.
pub(super) fn with_registry(cfg: &str, host: &str) -> Result<String, String> {
    let mut doc: DocumentMut = cfg
        .parse()
        .map_err(|e: TomlError| format!("is not valid TOML: {}", e.message()))?;
    let not_table = |key: &str| format!("sets `{key}` to something other than a table");
    let registry = child(doc.as_item_mut(), "registry").ok_or_else(|| not_table("registry"))?;
    let entry = child(registry, host).ok_or_else(|| not_table(&format!("registry.\"{host}\"")))?;
    set(
        entry,
        "ca",
        Value::Array(Array::from_iter(["/certs/ca.crt"])),
    );
    let mut pair = InlineTable::new();
    pair.insert("key", "/certs/client.key".into());
    pair.insert("cert", "/certs/client.crt".into());
    set(entry, "keypair", Value::Array(Array::from_iter([pair])));
    Ok(doc.to_string())
}

/// The table `key` of table `parent`, created (as a table of the same kind)
/// when missing. `None` when either is not a table.
fn child<'a>(parent: &'a mut Item, key: &str) -> Option<&'a mut Item> {
    match parent {
        Item::Table(t) => {
            let item = t.entry(key).or_insert_with(|| {
                let mut new = Table::new();
                new.set_implicit(true);
                Item::Table(new)
            });
            item.is_table_like().then_some(item)
        }
        Item::Value(Value::InlineTable(t)) => {
            if !t.contains_key(key) {
                t.insert(key, Value::InlineTable(InlineTable::new()));
            }
            let (_, item) = t.get_key_value_mut(key)?;
            item.is_table_like().then_some(item)
        }
        _ => None,
    }
}

/// Set `key` of the table `item` (a table [`child`] returned) to `v`.
fn set(item: &mut Item, key: &str, v: Value) {
    if let Some(t) = item.as_table_like_mut() {
        t.remove(key);
        t.insert(key, Item::Value(v));
    }
}

/// The scope id of a scope name: the first 32 hex chars of its SHA-256, so
/// no caller identity is written to the cluster in clear.
#[must_use]
pub fn scope_id(name: &str) -> String {
    sha256_hex(name.as_bytes())[..32].to_owned()
}

/// Whether `s` is a scope id [`scope_id`] could have made: 32 lowercase hex characters.
#[must_use]
pub fn is_scope_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Bytes of a Kubernetes quantity with an optional binary or decimal suffix.
/// `None` for forms a size never takes (exponents, milli-units).
#[must_use]
pub fn quantity_bytes(q: &str) -> Option<u64> {
    const UNITS: [(&str, u64); 12] = [
        ("Ki", 1 << 10),
        ("Mi", 1 << 20),
        ("Gi", 1 << 30),
        ("Ti", 1 << 40),
        ("Pi", 1 << 50),
        ("Ei", 1 << 60),
        ("k", 1_000),
        ("M", 1_000_000),
        ("G", 1_000_000_000),
        ("T", 1_000_000_000_000),
        ("P", 1_000_000_000_000_000),
        ("E", 1_000_000_000_000_000_000),
    ];
    let (num, mult) = UNITS
        .iter()
        .find_map(|(s, m)| q.strip_suffix(s).map(|n| (n, *m)))
        .unwrap_or((q, 1));
    let (int, frac) = num.split_once('.').unwrap_or((num, ""));
    if int.is_empty()
        || !int.bytes().all(|c| c.is_ascii_digit())
        || !frac.bytes().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let whole = int.parse::<u64>().ok()?.checked_mul(mult)?;
    let scale = 10u64.checked_pow(u32::try_from(frac.len()).ok()?)?;
    let part = if frac.is_empty() {
        0
    } else {
        frac.parse::<u64>().ok()?.checked_mul(mult)? / scale
    };
    whole.checked_add(part)
}
