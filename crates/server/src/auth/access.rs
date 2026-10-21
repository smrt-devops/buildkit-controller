//! Pool access decision for an authenticated [`Identity`].
//!
//! These are plain, validated mirror types of `spec.access.rules`; the
//! reconciler maps the CRD into them with [`AccessRules::new`], which repeats
//! the CRD's validation so an invalid rule can never reach [`AccessRules::allows`].

use std::collections::BTreeMap;

use serde_json::Value;

use super::Identity;

const MAX_RULES: usize = 64;
const MAX_CLAIMS: usize = 16;
const MAX_CLAIM_VALUE: usize = 256;
const MAX_ISSUER: usize = 512;

/// Why a rule set was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleError {
    #[error("more than {MAX_RULES} access rules")]
    TooManyRules,
    #[error("oidc issuer must start with https:// and be at most {MAX_ISSUER} bytes")]
    BadIssuer,
    #[error("oidc rule needs between 1 and {MAX_CLAIMS} claims")]
    ClaimCount,
    #[error(
        "claim {0:?}: value must be 1..={MAX_CLAIM_VALUE} characters and contain a character other than '*'"
    )]
    BadClaimValue(String),
    #[error("serviceAccount {0:?} is not a valid DNS name")]
    BadServiceAccount(String),
    #[error("claims rule needs between 1 and {MAX_CLAIM_NAMESPACES} DNS-1123 namespaces")]
    BadClaimNamespaces,
}

/// Most namespaces in one `claims` rule.
pub const MAX_CLAIM_NAMESPACES: usize = 32;

/// One `spec.access.rules[]` entry with exactly one of `oidc`, `serviceAccount` or `claims`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccessRule {
    Oidc {
        issuer: String,
        claims: BTreeMap<String, String>,
    },
    ServiceAccount {
        /// `None` = the pool's namespace.
        namespace: Option<String>,
        /// `None` = any service account in that namespace.
        name: Option<String>,
    },
    /// `BuildKitClaim` objects in any of `namespaces`.
    Claims { namespaces: Vec<String> },
}

/// A validated rule list. Empty means deny all.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccessRules(Vec<AccessRule>);

impl AccessRules {
    /// Validates `rules` against the CRD schema.
    pub fn new(rules: Vec<AccessRule>) -> Result<Self, RuleError> {
        if rules.len() > MAX_RULES {
            return Err(RuleError::TooManyRules);
        }
        rules.iter().try_for_each(validate)?;
        Ok(Self(rules))
    }

    /// True if any rule matches `who` for a pool in `pool_namespace`.
    #[must_use]
    pub fn allows(&self, who: &Identity, pool_namespace: &str) -> bool {
        self.0.iter().any(|rule| matches(rule, who, pool_namespace))
    }
}

fn validate(rule: &AccessRule) -> Result<(), RuleError> {
    match rule {
        AccessRule::Oidc { issuer, claims } => {
            if !issuer.starts_with("https://") || issuer.len() > MAX_ISSUER {
                return Err(RuleError::BadIssuer);
            }
            if claims.is_empty() || claims.len() > MAX_CLAIMS {
                return Err(RuleError::ClaimCount);
            }
            for (name, pattern) in claims {
                if pattern.chars().count() > MAX_CLAIM_VALUE || pattern.chars().all(|c| c == '*') {
                    return Err(RuleError::BadClaimValue(name.clone()));
                }
            }
            Ok(())
        }
        AccessRule::ServiceAccount { namespace, name } => {
            if let Some(ns) = namespace.as_deref().filter(|ns| !is_dns_label(ns)) {
                return Err(RuleError::BadServiceAccount(ns.to_owned()));
            }
            if let Some(n) = name.as_deref().filter(|n| !is_dns_subdomain(n)) {
                return Err(RuleError::BadServiceAccount(n.to_owned()));
            }
            Ok(())
        }
        AccessRule::Claims { namespaces } => {
            if namespaces.is_empty()
                || namespaces.len() > MAX_CLAIM_NAMESPACES
                || !namespaces.iter().all(|n| is_dns_label(n))
            {
                return Err(RuleError::BadClaimNamespaces);
            }
            Ok(())
        }
    }
}

fn matches(rule: &AccessRule, who: &Identity, pool_namespace: &str) -> bool {
    match (rule, who) {
        (
            AccessRule::Oidc { issuer, claims },
            Identity::Oidc {
                issuer: iss,
                claims: have,
                ..
            },
        ) => {
            issuer == iss
                && claims
                    .iter()
                    .all(|(name, pattern)| claim_matches(have.get(name), pattern))
        }
        (
            AccessRule::ServiceAccount { namespace, name },
            Identity::ServiceAccount {
                namespace: ns,
                name: sa,
                ..
            },
        ) => {
            namespace.as_deref().unwrap_or(pool_namespace) == ns
                && name.as_deref().is_none_or(|n| n == sa)
        }
        (AccessRule::Claims { namespaces }, Identity::Claim { namespace, .. }) => {
            namespaces.iter().any(|n| n == namespace)
        }
        _ => false,
    }
}

fn claim_matches(value: Option<&Value>, pattern: &str) -> bool {
    match value {
        Some(Value::String(s)) => glob(pattern, s),
        Some(Value::Array(items)) => items
            .iter()
            .any(|v| v.as_str().is_some_and(|s| glob(pattern, s))),
        _ => false,
    }
}

/// `*` matches any sequence (including empty and `/`); everything else is literal.
fn glob(pattern: &str, text: &str) -> bool {
    let mut parts = pattern.split('*');
    let Some(first) = parts.next() else {
        return text.is_empty();
    };
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let middle: Vec<&str> = parts.collect();
    let Some((last, middle)) = middle.split_last() else {
        return rest.is_empty(); // no '*' at all, so match exactly
    };
    for part in middle {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

fn is_dns_label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !s.starts_with('-')
        && !s.ends_with('-')
}

fn is_dns_subdomain(s: &str) -> bool {
    s.len() <= 253 && s.split('.').all(is_dns_label)
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, json};

    use super::*;

    const GH: &str = "https://token.actions.githubusercontent.com";

    fn oidc(iss: &str, claims: Value) -> Identity {
        let Value::Object(claims) = claims else {
            panic!("object")
        };
        Identity::Oidc {
            issuer: iss.into(),
            subject: "s".into(),
            claims,
        }
    }

    fn sa(ns: &str, name: &str) -> Identity {
        Identity::ServiceAccount {
            namespace: ns.into(),
            name: name.into(),
            uid: "u".into(),
        }
    }

    fn rule(claims: &[(&str, &str)]) -> AccessRule {
        AccessRule::Oidc {
            issuer: GH.into(),
            claims: claims
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    fn sa_rule(ns: Option<&str>, name: Option<&str>) -> AccessRule {
        AccessRule::ServiceAccount {
            namespace: ns.map(Into::into),
            name: name.map(Into::into),
        }
    }

    #[test]
    fn default_deny() {
        let rules = AccessRules::default();
        assert!(!rules.allows(&sa("ci", "builder"), "ci"));
        assert!(!rules.allows(&oidc(GH, json!({"repository_owner": "x"})), "ci"));
    }

    #[test]
    fn validation_rejects_bad_rules() {
        let cases = [
            (rule(&[]), RuleError::ClaimCount),
            (
                rule(&[("a", &"\u{e9}".repeat(257))]),
                RuleError::BadClaimValue("a".into()),
            ),
            (rule(&[("a", "*")]), RuleError::BadClaimValue("a".into())),
            (rule(&[("a", "")]), RuleError::BadClaimValue("a".into())),
            (
                AccessRule::Oidc {
                    issuer: "http://x".into(),
                    claims: [("a".into(), "b".into())].into(),
                },
                RuleError::BadIssuer,
            ),
            (
                sa_rule(Some("Bad_NS"), None),
                RuleError::BadServiceAccount("Bad_NS".into()),
            ),
        ];
        for (r, want) in cases {
            assert_eq!(AccessRules::new(vec![r]), Err(want));
        }
        // maxLength counts characters, so 256 two-byte characters are accepted.
        AccessRules::new(vec![rule(&[("a", &"\u{e9}".repeat(256))])]).unwrap();
        let many: Vec<_> = (0..17).map(|i| (format!("c{i}"), "v".to_owned())).collect();
        let r = AccessRule::Oidc {
            issuer: GH.into(),
            claims: many.into_iter().collect(),
        };
        assert_eq!(AccessRules::new(vec![r]), Err(RuleError::ClaimCount));
        let too_many = vec![sa_rule(None, None); 65];
        assert_eq!(AccessRules::new(too_many), Err(RuleError::TooManyRules));
    }

    #[test]
    fn oidc_claim_matching_table() {
        let id = oidc(
            GH,
            json!({
                "repository": "smrt-devops/app",
                "repository_owner": "smrt-devops",
                "ref": "refs/heads/main",
                "groups": ["dev", "ops"],
                "run_attempt": 1,
                "nested": {"a": "b"},
            }),
        );
        let cases: &[(&[(&str, &str)], bool)] = &[
            (&[("repository_owner", "smrt-devops")], true),
            (&[("repository_owner", "smrt-devop")], false),
            (&[("repository_owner", "SMRT-DEVOPS")], false),
            (&[("repository", "smrt-devops/*")], true),
            (&[("repository", "*/app")], true),
            (&[("repository", "smrt-*-app")], false),
            (&[("repository", "smrt*app")], true),
            (&[("ref", "refs/*/main")], true),
            (&[("ref", "refs/heads/main*")], true),
            (&[("ref", "?efs/heads/main")], false),
            (&[("ref", "refs/heads/[m]ain")], false),
            (&[("groups", "ops")], true),
            (&[("groups", "o*")], true),
            (&[("groups", "admin")], false),
            (&[("run_attempt", "1")], false),
            (&[("nested", "*b*")], false),
            (&[("missing", "x*")], false),
            (
                &[
                    ("repository_owner", "smrt-devops"),
                    ("ref", "refs/heads/dev"),
                ],
                false,
            ),
            (
                &[("repository_owner", "smrt-devops"), ("ref", "refs/heads/*")],
                true,
            ),
        ];
        for (claims, want) in cases {
            let rules = AccessRules::new(vec![rule(claims)]).unwrap();
            assert_eq!(rules.allows(&id, "ci"), *want, "{claims:?}");
        }
    }

    #[test]
    fn oidc_issuer_must_match_exactly() {
        let rules = AccessRules::new(vec![rule(&[("sub", "*x*")])]).unwrap();
        assert!(rules.allows(&oidc(GH, json!({"sub": "x"})), "ci"));
        assert!(!rules.allows(&oidc(&format!("{GH}/"), json!({"sub": "x"})), "ci"));
        assert!(!rules.allows(&sa("ci", "x"), "ci"));
    }

    #[test]
    fn service_account_matching_table() {
        let cases = [
            (sa_rule(None, None), sa("ci", "any"), true),
            (sa_rule(None, None), sa("other", "any"), false),
            (sa_rule(None, Some("builder")), sa("ci", "builder"), true),
            (sa_rule(None, Some("builder")), sa("ci", "builder2"), false),
            (
                sa_rule(Some("tools"), Some("builder")),
                sa("tools", "builder"),
                true,
            ),
            (sa_rule(Some("tools"), None), sa("ci", "builder"), false),
            (
                sa_rule(None, None),
                oidc(GH, Value::Object(Map::new())),
                false,
            ),
        ];
        for (r, who, want) in cases {
            let rules = AccessRules::new(vec![r.clone()]).unwrap();
            assert_eq!(rules.allows(&who, "ci"), want, "{r:?} {who:?}");
        }
    }

    #[test]
    fn glob_edges() {
        assert!(glob("a*", "a"));
        assert!(glob("*a", "a"));
        assert!(glob("a**b", "ab"));
        assert!(glob("a*b*c", "abbc"));
        assert!(!glob("a*b*c", "acb"));
        assert!(!glob("ab", "abc"));
        assert!(glob("*é*", "café!"));
    }

    fn claim(ns: &str) -> Identity {
        Identity::Claim {
            namespace: ns.into(),
            name: "c".into(),
            uid: "u".into(),
        }
    }

    #[test]
    fn claims_rule_matches_claim_namespace_only() {
        let rules = AccessRules::new(vec![AccessRule::Claims {
            namespaces: vec!["ci".into()],
        }])
        .unwrap();
        assert!(rules.allows(&claim("ci"), "pools"));
        assert!(!rules.allows(&claim("other"), "pools"));
        // A claims rule never matches a token identity from the same namespace.
        assert!(!rules.allows(&sa("ci", "builder"), "pools"));
        // Token rules never match a claim, even one in the pool's own namespace.
        let sa_rules = AccessRules::new(vec![sa_rule(None, None)]).unwrap();
        assert!(!sa_rules.allows(&claim("pools"), "pools"));
        assert_eq!(claim("ci").owner(), "claim:ci/c#u");
    }

    #[test]
    fn claims_rule_validation() {
        for bad in [
            vec![],
            vec!["Bad".to_owned()],
            (0..33).map(|i| format!("n{i}")).collect(),
        ] {
            assert!(AccessRules::new(vec![AccessRule::Claims { namespaces: bad }]).is_err());
        }
    }
}
