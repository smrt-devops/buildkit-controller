//! Verify-only `jsonwebtoken` crypto provider backed by `ring`.
//!
//! `jsonwebtoken` 11 needs a process-wide provider. Its bundled ones pull in
//! `rsa` (RUSTSEC-2023-0071) or `aws-lc-sys` (OpenSSL licence), both refused by
//! `deny.toml`, so this module wires `ring` (already used by rustls) instead.
//! Only the algorithms the API accepts have a verifier; HMAC and every signer
//! are refused, so a token cannot be checked against a shared secret even if a
//! caller were to build one.

use std::sync::OnceLock;

use jsonwebtoken::crypto::{CryptoProvider, JwtSigner, JwtVerifier, KeyUtils};
use jsonwebtoken::errors::{Error, ErrorKind};
use jsonwebtoken::{Algorithm, AlgorithmFamily, DecodingKey, DecodingKeyKind, EncodingKey};
use ring::signature::{self, RsaPublicKeyComponents, UnparsedPublicKey, VerificationAlgorithm};

/// Algorithms accepted for OIDC tokens.
pub(crate) const ALLOWED_ALGS: [Algorithm; 9] = [
    Algorithm::RS256,
    Algorithm::RS384,
    Algorithm::RS512,
    Algorithm::PS256,
    Algorithm::PS384,
    Algorithm::PS512,
    Algorithm::ES256,
    Algorithm::ES384,
    Algorithm::EdDSA,
];

static PROVIDER: CryptoProvider = CryptoProvider {
    signer_factory: refuse_signer,
    verifier_factory: new_verifier,
    key_utils: KeyUtils::new_unimplemented(),
};

/// Installs the ring provider once per process.
///
/// Returns `false` if a different provider was installed first; callers then
/// fail closed rather than verify with a backend this module does not control.
pub(crate) fn install() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(|| match PROVIDER.install_default() {
        Ok(()) => true,
        Err(existing) => std::ptr::eq(existing, &raw const PROVIDER),
    })
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "signature fixed by CryptoProvider"
)]
fn refuse_signer(_: &Algorithm, _: &EncodingKey) -> Result<Box<dyn JwtSigner>, Error> {
    Err(ErrorKind::InvalidAlgorithm.into())
}

enum Key {
    Rsa { n: Vec<u8>, e: Vec<u8> },
    Raw(Vec<u8>),
}

struct RingVerifier {
    alg: Algorithm,
    ring_alg: &'static dyn VerificationAlgorithm,
    rsa: Option<&'static signature::RsaParameters>,
    key: Key,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "signature fixed by CryptoProvider"
)]
fn new_verifier(alg: &Algorithm, key: &DecodingKey) -> Result<Box<dyn JwtVerifier>, Error> {
    use Algorithm as A;
    let (family, ring_alg, rsa): (_, &'static dyn VerificationAlgorithm, _) = match alg {
        A::RS256 => rsa(&signature::RSA_PKCS1_2048_8192_SHA256),
        A::RS384 => rsa(&signature::RSA_PKCS1_2048_8192_SHA384),
        A::RS512 => rsa(&signature::RSA_PKCS1_2048_8192_SHA512),
        A::PS256 => rsa(&signature::RSA_PSS_2048_8192_SHA256),
        A::PS384 => rsa(&signature::RSA_PSS_2048_8192_SHA384),
        A::PS512 => rsa(&signature::RSA_PSS_2048_8192_SHA512),
        A::ES256 => (
            AlgorithmFamily::Ec,
            &signature::ECDSA_P256_SHA256_FIXED,
            None,
        ),
        A::ES384 => (
            AlgorithmFamily::Ec,
            &signature::ECDSA_P384_SHA384_FIXED,
            None,
        ),
        A::EdDSA => (AlgorithmFamily::Ed, &signature::ED25519, None),
        _ => return Err(ErrorKind::InvalidAlgorithm.into()), // HS* and anything new
    };
    if key.family() != family {
        return Err(ErrorKind::InvalidAlgorithm.into());
    }
    let key = match (key.kind(), rsa.is_some()) {
        (DecodingKeyKind::RsaModulusExponent { n, e }, true) => Key::Rsa {
            n: n.clone(),
            e: e.clone(),
        },
        (DecodingKeyKind::SecretOrDer(raw), false) => Key::Raw(raw.clone()),
        _ => return Err(ErrorKind::InvalidKeyFormat.into()),
    };
    Ok(Box::new(RingVerifier {
        alg: *alg,
        ring_alg,
        rsa,
        key,
    }))
}

fn rsa(
    params: &'static signature::RsaParameters,
) -> (
    AlgorithmFamily,
    &'static dyn VerificationAlgorithm,
    Option<&'static signature::RsaParameters>,
) {
    (AlgorithmFamily::Rsa, params, Some(params))
}

impl jsonwebtoken::signature::Verifier<Vec<u8>> for RingVerifier {
    fn verify(&self, msg: &[u8], sig: &Vec<u8>) -> Result<(), jsonwebtoken::signature::Error> {
        let ok = match (&self.key, self.rsa) {
            (Key::Rsa { n, e }, Some(params)) => RsaPublicKeyComponents { n, e }
                .verify(params, msg, sig)
                .is_ok(),
            (Key::Raw(raw), None) => UnparsedPublicKey::new(self.ring_alg, raw)
                .verify(msg, sig)
                .is_ok(),
            _ => false,
        };
        if ok {
            Ok(())
        } else {
            Err(jsonwebtoken::signature::Error::new())
        }
    }
}

impl JwtVerifier for RingVerifier {
    fn algorithm(&self) -> Algorithm {
        self.alg
    }
}
