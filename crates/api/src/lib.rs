//! API types for buildkit-controller. The crate holds the `BuildKitPool` CRD, the HTTP API DTOs
//! and, behind the `client` feature, a typed async client.

pub mod claim;
pub mod dto;
pub mod v1alpha1;

#[cfg(feature = "client")]
pub mod client;

/// Errors raised while validating API types.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ApiError {
    /// A field failed validation.
    #[error("invalid field `{0}`")]
    InvalidField(&'static str),
    /// A field failed validation, with the reason.
    #[error("invalid field `{field}`: {reason}")]
    Invalid {
        /// Dotted field path.
        field: String,
        /// What is wrong.
        reason: String,
    },
}
