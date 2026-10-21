//! The e2e harness's mock OIDC issuer. It holds an ES256 key and the issuer's
//! files (discovery document, JWKS, and a self-signed serving certificate).
//! `socat` serves them from inside the cluster (see
//! `xtask/fixtures/e2e/mock-oidc.yaml`), because a listener on the host is not
//! reachable from kind on every Docker (`OrbStack`, macOS firewall). The
//! certificate is the issuer's `caBundleConfigMap`.

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use ring::rand::SystemRandom;
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use serde_json::json;
use time::OffsetDateTime;

pub(crate) struct Issuer {
    /// The issuer URL (`iss`), as the cluster dials it.
    pub(crate) url: String,
    /// PEM of the self-signed serving certificate (also the trust anchor).
    pub(crate) ca_pem: String,
    /// PEM of the serving key.
    pub(crate) key_pem: String,
    /// The discovery document.
    pub(crate) discovery: String,
    /// The JWKS document.
    pub(crate) jwks: String,
    key: EcdsaKeyPair,
}

impl Issuer {
    /// An issuer at `https://<host>`; `host` is the in-cluster DNS name.
    pub(crate) fn new(host: &str) -> Result<Self> {
        let rng = SystemRandom::new();
        let der = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng)
            .map_err(|e| anyhow::anyhow!("keygen: {e}"))?;
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, der.as_ref(), &rng)
            .map_err(|e| anyhow::anyhow!("key: {e}"))?;
        let p = key.public_key().as_ref();
        let jwks = json!({"keys": [{"kty": "EC", "crv": "P-256", "kid": "k1", "use": "sig",
            "alg": "ES256", "x": B64.encode(&p[1..33]), "y": B64.encode(&p[33..65])}]});
        let url = format!("https://{host}");
        let discovery = json!({"issuer": url, "jwks_uri": format!("{url}/jwks")});
        let c = rcgen::generate_simple_self_signed(vec![host.to_owned()])
            .context("issuer certificate")?;
        Ok(Self {
            url,
            ca_pem: c.cert.pem(),
            key_pem: c.signing_key.serialize_pem(),
            discovery: discovery.to_string(),
            jwks: jwks.to_string(),
            key,
        })
    }

    /// A fresh ten-minute token for `aud` carrying `team`.
    pub(crate) fn token(&self, aud: &str, team: &str) -> Result<String> {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let header = json!({"alg": "ES256", "typ": "JWT", "kid": "k1"});
        let claims = json!({"iss": self.url, "sub": format!("user-{team}"), "aud": aud,
            "iat": now, "nbf": now - 5, "exp": now + 600, "team": team});
        let msg = format!(
            "{}.{}",
            B64.encode(header.to_string()),
            B64.encode(claims.to_string())
        );
        let sig = self
            .key
            .sign(&SystemRandom::new(), msg.as_bytes())
            .map_err(|e| anyhow::anyhow!("sign: {e}"))?;
        Ok(format!("{msg}.{}", B64.encode(sig.as_ref())))
    }
}
