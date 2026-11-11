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
const MAX_EXCLUDE_PATTERNS: usize = 16;

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
        "claim {0:?}: value must be 1..={MAX_CLAIM_VALUE} characters, and a value made only of '*' needs another claim in the rule whose value has a character other than '*'"
    )]
    BadClaimValue(String),
    #[error(
        "exclude needs at most {MAX_CLAIMS} claims, each with 1 to {MAX_EXCLUDE_PATTERNS} patterns of 1..={MAX_CLAIM_VALUE} characters that are not only '*'"
    )]
    BadExclude,
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
        /// Claim name to patterns; a match on any of them refuses the rule.
        exclude: BTreeMap<String, Vec<String>>,
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
        AccessRule::Oidc {
            issuer,
            claims,
            exclude,
        } => {
            if !issuer.starts_with("https://") || issuer.len() > MAX_ISSUER {
                return Err(RuleError::BadIssuer);
            }
            if claims.is_empty() || claims.len() > MAX_CLAIMS {
                return Err(RuleError::ClaimCount);
            }
            for (name, pattern) in claims {
                if pattern.is_empty() || pattern.chars().count() > MAX_CLAIM_VALUE {
                    return Err(RuleError::BadClaimValue(name.clone()));
                }
            }
            // A presence-only claim is allowed only beside a claim pinned to something.
            if claims.values().all(|p| is_presence(p)) {
                let first = claims.keys().next().cloned().unwrap_or_default();
                return Err(RuleError::BadClaimValue(first));
            }
            if exclude.len() > MAX_CLAIMS
                || exclude.values().any(|ps| {
                    ps.is_empty()
                        || ps.len() > MAX_EXCLUDE_PATTERNS
                        || ps.iter().any(|p| {
                            p.is_empty() || p.chars().count() > MAX_CLAIM_VALUE || is_presence(p)
                        })
                })
            {
                return Err(RuleError::BadExclude);
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
            AccessRule::Oidc {
                issuer,
                claims,
                exclude,
            },
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
                && !exclude.iter().any(|(name, patterns)| {
                    patterns.iter().any(|p| claim_matches(have.get(name), p))
                })
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
        Some(Value::String(s)) => text_matches(pattern, s),
        Some(Value::Array(items)) => items
            .iter()
            .any(|v| v.as_str().is_some_and(|s| text_matches(pattern, s))),
        _ => false,
    }
}

/// A value made only of `*` means "present": any non-empty string. Anything else is a glob.
fn text_matches(pattern: &str, text: &str) -> bool {
    if is_presence(pattern) {
        !text.is_empty()
    } else {
        glob(pattern, text)
    }
}

fn is_presence(pattern: &str) -> bool {
    !pattern.is_empty() && pattern.chars().all(|c| c == '*')
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
            pod_uid: None,
        }
    }

    fn rule(claims: &[(&str, &str)]) -> AccessRule {
        AccessRule::Oidc {
            issuer: GH.into(),
            claims: claims
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect(),
            exclude: BTreeMap::new(),
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
                rule(&[("a", "*"), ("b", "**")]),
                RuleError::BadClaimValue("a".into()),
            ),
            (
                AccessRule::Oidc {
                    issuer: "http://x".into(),
                    claims: [("a".into(), "b".into())].into(),
                    exclude: BTreeMap::new(),
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
            exclude: BTreeMap::new(),
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
    fn presence_claim_needs_a_pinned_claim_and_a_non_empty_string() {
        let r = [("repository_owner_id", "1"), ("repo_property_domain", "*")];
        let rules = AccessRules::new(vec![rule(&r)]).unwrap();
        let tok = |owner: &str, extra: Value| {
            let mut c = json!({"repository_owner_id": owner});
            c.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            oidc(GH, c)
        };
        let domain = |v: Value| tok("1", json!({"repo_property_domain": v}));
        assert!(rules.allows(&domain(json!("payments")), "ci"));
        assert!(rules.allows(&domain(json!(["", "payments"])), "ci"));
        for v in [
            json!(""),
            json!([""]),
            json!(1),
            json!(true),
            json!(null),
            json!({"a": "b"}),
            json!([1]),
        ] {
            assert!(!rules.allows(&domain(v.clone()), "ci"), "{v}");
        }
        assert!(!rules.allows(&tok("1", json!({})), "ci"));
        assert!(!rules.allows(&tok("2", json!({"repo_property_domain": "x"})), "ci"));
        // `**` is the same as `*`.
        let r = [("repository_owner_id", "1"), ("repo_property_domain", "**")];
        let rules = AccessRules::new(vec![rule(&r)]).unwrap();
        assert!(rules.allows(&domain(json!("x")), "ci"));
        assert!(!rules.allows(&domain(json!("")), "ci"));
        // Presence alone is refused; so is a rule whose values are all wildcards.
        for r in [vec![("a", "*")], vec![("a", "*"), ("b", "**")]] {
            assert!(matches!(
                AccessRules::new(vec![rule(&r)]),
                Err(RuleError::BadClaimValue(_))
            ));
        }
        // `*` inside a value still matches the empty run.
        let rules = AccessRules::new(vec![rule(&[("a", "x*")])]).unwrap();
        assert!(rules.allows(&oidc(GH, json!({"a": "x"})), "ci"));
    }

    fn excluding(excl: &[(&str, &[&str])]) -> AccessRule {
        let AccessRule::Oidc { issuer, claims, .. } =
            rule(&[("repository_owner_id", "1"), ("repo_property_domain", "*")])
        else {
            panic!("oidc")
        };
        let exclude = excl
            .iter()
            .map(|(k, ps)| {
                (
                    (*k).to_owned(),
                    ps.iter().map(|p| (*p).to_owned()).collect(),
                )
            })
            .collect();
        AccessRule::Oidc {
            issuer,
            claims,
            exclude,
        }
    }

    #[test]
    fn exclude_refuses_matching_claims_and_never_admits_alone() {
        let rules = AccessRules::new(vec![excluding(&[(
            "repo_property_domain",
            &["pci", "x-*"],
        )])])
        .unwrap();
        let tok = |owner: &str, d: Value| {
            oidc(
                GH,
                json!({"repository_owner_id": owner, "repo_property_domain": d}),
            )
        };
        assert!(rules.allows(&tok("1", json!("payments")), "ci"));
        assert!(!rules.allows(&tok("1", json!("pci")), "ci"));
        assert!(!rules.allows(&tok("1", json!("x-eu")), "ci"));
        assert!(!rules.allows(&tok("2", json!("payments")), "ci"));
        assert!(!rules.allows(&tok("1", json!(["payments", "pci"])), "ci"));
        assert!(rules.allows(&tok("1", json!(["payments", "web"])), "ci"));
        // A missing domain fails the claims entry; exclude does not rescue it.
        assert!(!rules.allows(&oidc(GH, json!({"repository_owner_id": "1"})), "ci"));
        // A rule whose claims match nobody admits nobody, with or without exclude.
        let mut r = excluding(&[("repo_property_domain", &["pci"])]);
        if let AccessRule::Oidc { claims, .. } = &mut r {
            claims.insert("repository_owner_id".into(), "9".into());
        }
        let rules = AccessRules::new(vec![r]).unwrap();
        assert!(!rules.allows(&tok("1", json!("payments")), "ci"));
        // An excluded claim that is absent or not a string excludes nothing.
        let rules = AccessRules::new(vec![excluding(&[("other", &["x"])])]).unwrap();
        assert!(rules.allows(&tok("1", json!("payments")), "ci"));
    }

    #[test]
    fn exclude_validation() {
        let many: Vec<&str> = vec!["a"; 17];
        let long = "x".repeat(257);
        let bad: Vec<AccessRule> = vec![
            excluding(&[("a", &["*"])]),
            excluding(&[("a", &["**"])]),
            excluding(&[("a", &[])]),
            excluding(&[("a", &[""])]),
            excluding(&[("a", &many)]),
            excluding(&[("a", &[long.as_str()])]),
        ];
        for r in bad {
            assert_eq!(AccessRules::new(vec![r]), Err(RuleError::BadExclude));
        }
        AccessRules::new(vec![excluding(&[("a", &["*x*"])])]).unwrap();
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
