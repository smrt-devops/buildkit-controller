//! Golden tests check that DTO JSON matches the documented API examples exactly.
#![allow(clippy::unwrap_used)]

use buildkit_controller_api::dto::{
    AllocationResponse, CreateAllocationRequest, ErrorBody, ErrorCode, PoolList, PoolSummary,
};
use serde_json::{Value, json};

fn roundtrip<T>(text: &str, value: &T)
where
    T: serde::Serialize + serde::de::DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let expected: Value = serde_json::from_str(text).unwrap();
    assert_eq!(&serde_json::from_str::<T>(text).unwrap(), value);
    assert_eq!(serde_json::to_value(value).unwrap(), expected);
}

#[test]
fn create_request() {
    roundtrip(
        r#"{"namespace": "ci", "pool": "default", "ttlSeconds": 3600, "queueTimeoutSeconds": 300, "publicKey": "-----BEGIN PUBLIC KEY-----...", "cacheKey": "org-app.main_1", "cache": "none"}"#,
        &CreateAllocationRequest {
            namespace: "ci".into(),
            pool: "default".into(),
            ttl_seconds: Some(3600),
            queue_timeout_seconds: Some(300),
            public_key: Some("-----BEGIN PUBLIC KEY-----...".into()),
            cache_key: Some("org-app.main_1".to_owned().try_into().unwrap()),
            cache: Some(buildkit_controller_api::dto::CacheChoice::None),
        },
    );
    let min: CreateAllocationRequest =
        serde_json::from_str(r#"{"namespace":"ci","pool":"p"}"#).unwrap();
    assert_eq!(
        serde_json::to_value(&min).unwrap(),
        json!({"namespace": "ci", "pool": "p"})
    );
}

#[test]
fn create_request_rejects_unknown_and_missing() {
    assert!(
        serde_json::from_str::<CreateAllocationRequest>(
            r#"{"namespace":"ci","pool":"p","extra":1}"#
        )
        .is_err()
    );
    assert!(serde_json::from_str::<CreateAllocationRequest>(r#"{"pool":"p"}"#).is_err());
    assert!(
        serde_json::from_str::<CreateAllocationRequest>(
            r#"{"namespace":"ci","pool":"p","ttlSeconds":-1}"#
        )
        .is_err()
    );
}

#[test]
fn cache_key_is_a_label_value() {
    let parse = |k: &str| {
        serde_json::from_value::<CreateAllocationRequest>(
            json!({"namespace": "ci", "pool": "p", "cacheKey": k}),
        )
        .map(|r| r.cache_key.unwrap().as_str().to_owned())
    };
    for ok in ["a", "A9", "org.app_main-1", &"k".repeat(63)] {
        assert_eq!(parse(ok).unwrap(), ok);
    }
    for bad in [
        "",
        "-a",
        "a-",
        ".a",
        "a_",
        "org/app",
        "a b",
        "ä",
        &"k".repeat(64),
    ] {
        let e = parse(bad).unwrap_err().to_string();
        assert!(e.contains("cacheKey"), "{bad:?}: {e}");
    }
}

fn response(key: Option<&str>, ca: Option<&str>) -> AllocationResponse {
    AllocationResponse {
        id: "k3q7m2x4b5n6p7r2s3t4v5w6y7".into(),
        namespace: "ci".into(),
        pool: "default".into(),
        owner: "sa:ci/builder".into(),
        endpoint: "tcp://bk.example.com:8444".into(),
        server_name: "bk.example.com".into(),
        ca_cert: ca.map(Into::into),
        client_cert: "-----BEGIN CERTIFICATE-----...".into(),
        client_key: key.map(Into::into),
        expires_at: "2026-10-19T13:00:00Z".into(),
        cache: None,
    }
}

#[test]
fn allocation_response() {
    roundtrip(
        r#"{
  "id": "k3q7m2x4b5n6p7r2s3t4v5w6y7",
  "namespace": "ci",
  "pool": "default",
  "owner": "sa:ci/builder",
  "endpoint": "tcp://bk.example.com:8444",
  "serverName": "bk.example.com",
  "caCert": "-----BEGIN CERTIFICATE-----...",
  "clientCert": "-----BEGIN CERTIFICATE-----...",
  "clientKey": "-----BEGIN PRIVATE KEY-----...",
  "expiresAt": "2026-10-19T13:00:00Z"
}"#,
        &response(
            Some("-----BEGIN PRIVATE KEY-----..."),
            Some("-----BEGIN CERTIFICATE-----..."),
        ),
    );
}

#[test]
fn allocation_response_null_ca_and_omitted_key() {
    let v = serde_json::to_value(response(None, None)).unwrap();
    assert_eq!(v["caCert"], Value::Null);
    assert!(v.as_object().unwrap().contains_key("caCert"));
    assert!(!v.as_object().unwrap().contains_key("clientKey"));
}

#[test]
fn pool_list() {
    roundtrip(
        r#"{"perAllocationEndpoints": true, "pools": [{"namespace": "ci", "name": "default", "ready": true, "warm": 2, "starting": 0,
            "allocated": 1, "waiting": 3, "min": 1, "max": 10, "platforms": ["linux/amd64"], "endpoint": "tcp://bk.example.com:8444",
            "serverName": "bk.example.com"}]}"#,
        &PoolList {
            per_allocation_endpoints: true,
            pools: vec![PoolSummary {
                namespace: "ci".into(),
                name: "default".into(),
                ready: true,
                warm: 2,
                starting: 0,
                allocated: 1,
                waiting: 3,
                min: 1,
                max: 10,
                platforms: vec!["linux/amd64".into()],
                endpoint: "tcp://bk.example.com:8444".into(),
                server_name: "bk.example.com".into(),
                queue_timeout_seconds: None,
                max_queue_timeout_seconds: None,
                default_ttl_seconds: None,
                max_ttl_seconds: None,
            }],
        },
    );
    // A current server adds the pool's queue and TTL limits; the body above,
    // from an older server, still parses.
    roundtrip(
        r#"{"namespace": "ci", "name": "default", "ready": true, "warm": 0, "starting": 0, "allocated": 0,
            "waiting": 0, "min": 0, "max": 1, "platforms": [], "endpoint": "tcp://bk.example.com:8444",
            "serverName": "bk.example.com", "queueTimeoutSeconds": 120, "maxQueueTimeoutSeconds": 600,
            "defaultTtlSeconds": 3600, "maxTtlSeconds": 86400}"#,
        &PoolSummary {
            namespace: "ci".into(),
            name: "default".into(),
            ready: true,
            warm: 0,
            starting: 0,
            allocated: 0,
            waiting: 0,
            min: 0,
            max: 1,
            platforms: Vec::new(),
            endpoint: "tcp://bk.example.com:8444".into(),
            server_name: "bk.example.com".into(),
            queue_timeout_seconds: Some(120),
            max_queue_timeout_seconds: Some(600),
            default_ttl_seconds: Some(3600),
            max_ttl_seconds: Some(86400),
        },
    );
    roundtrip(
        r#"{"perAllocationEndpoints": false, "pools": []}"#,
        &PoolList::default(),
    );
}

#[test]
fn error_body() {
    roundtrip(
        r#"{"error": {"code": "pool_exhausted", "message": "no warm pod and no free slot in ci/default"}}"#,
        &ErrorBody::new(
            ErrorCode::PoolExhausted,
            "no warm pod and no free slot in ci/default",
        ),
    );
}

#[test]
fn error_codes_match_spec_table() {
    let table = [
        (ErrorCode::InvalidRequest, "invalid_request", 400),
        (ErrorCode::Unauthenticated, "unauthenticated", 401),
        (ErrorCode::Forbidden, "forbidden", 403),
        (ErrorCode::PoolNotFound, "pool_not_found", 404),
        (ErrorCode::NotFound, "not_found", 404),
        (ErrorCode::PayloadTooLarge, "payload_too_large", 413),
        (
            ErrorCode::UnsupportedMediaType,
            "unsupported_media_type",
            415,
        ),
        (ErrorCode::Internal, "internal", 500),
        (ErrorCode::PoolExhausted, "pool_exhausted", 503),
        (ErrorCode::StartTimeout, "start_timeout", 503),
        (ErrorCode::PodLost, "pod_lost", 503),
        (ErrorCode::NotReady, "not_ready", 503),
        (ErrorCode::CaUnavailable, "ca_unavailable", 503),
        (ErrorCode::UpstreamError, "upstream_error", 503),
        (ErrorCode::ShuttingDown, "shutting_down", 503),
    ];
    assert_eq!(table.len(), ErrorCode::ALL.len());
    for (code, s, status) in table {
        assert_eq!(code.as_str(), s);
        assert_eq!(code.to_string(), s);
        assert_eq!(code.http_status(), status);
        assert_eq!(serde_json::to_value(code).unwrap(), json!(s));
        assert!(ErrorCode::ALL.contains(&code));
    }
    assert_eq!(ErrorCode::PoolExhausted.retry_after_seconds(), Some(5));
    assert_eq!(ErrorCode::StartTimeout.retry_after_seconds(), Some(5));
    assert_eq!(ErrorCode::NotReady.retry_after_seconds(), None);
    assert_eq!(ErrorCode::ShuttingDown.retry_after_seconds(), Some(1));
    assert_eq!(ErrorCode::UpstreamError.retry_after_seconds(), Some(2));
}
