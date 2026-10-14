use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use sha3::{Digest, Keccak256};
use std::{error::Error, fmt};

/// Sourcify v2 uses a flat error object. Operational Arkiv errors use the same
/// envelope, with explicit service-specific codes rather than false 404s.
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    body: Option<Value>,
}
impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            body: None,
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_parameter", message)
    }
    pub fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend_unavailable",
            "The configured Arkiv backend is unavailable",
        )
    }
    pub fn corrupt() -> Self {
        Self::new(
            StatusCode::BAD_GATEWAY,
            "corrupt_data",
            "The stored record or its dependencies failed validation",
        )
    }
    pub fn deadline() -> Self {
        Self::new(
            StatusCode::GATEWAY_TIMEOUT,
            "deadline_exceeded",
            "The contract read exceeded its deadline",
        )
    }
    pub fn from_read(error: anyhow::Error) -> Self {
        if let Some(api) = error.downcast_ref::<Self>() {
            return api.clone();
        }
        use crate::backend::BackendError;
        let mapped = if let Some(backend) = error.downcast_ref::<BackendError>() {
            match backend {
                BackendError::Unavailable(_) => Self::unavailable(),
                BackendError::MissingDependency(_) => Self::new(
                    StatusCode::BAD_GATEWAY,
                    "missing_dependency",
                    "A required stored dependency is missing",
                ),
                BackendError::Ambiguous(_) => Self::new(
                    StatusCode::BAD_GATEWAY,
                    "ambiguous_record",
                    "Multiple stored records match this lookup",
                ),
                BackendError::WorkLimit(_) => Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "work_limit",
                    "The configured dependency work limit was exceeded",
                ),
                BackendError::StateChanged(_) => Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "state_changed",
                    "The Arkiv state changed during the contract read",
                ),
                BackendError::Corrupt(_) => Self::corrupt(),
            }
        } else {
            Self::corrupt()
        };
        tracing::warn!(code = mapped.code, error = %error, "contract read failed");
        mapped
    }
}
impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl Error for ApiError {}
impl ApiError {
    pub(crate) fn response_body(self) -> Value {
        self.body.unwrap_or_else(|| {
            json!({
                "customCode": self.code,
                "message": self.message,
                "errorId": uuid::Uuid::new_v4().to_string(),
            })
        })
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = self.status;
        let body = self.response_body();
        (
            status,
            [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
            Json(body),
        )
            .into_response()
    }
}

pub fn not_found(address: &str) -> ApiError {
    let mut error = ApiError::new(
        StatusCode::NOT_FOUND,
        "contract_not_found",
        "Contract is absent from the configured Arkiv dataset",
    );
    error.body = Some(json!({
        "match": null, "creationMatch": null, "runtimeMatch": null,
        "chainId": "1", "address": checksum_address(address).unwrap_or_else(|| address.to_owned()),
    }));
    error
}

pub fn validate_path(chain: &str, address: &str) -> Result<String, ApiError> {
    // Mirror the OpenAPI schema checks, which run before Ethers normalization.
    if chain.len() > 20 {
        return Err(ApiError::invalid(
            "request/params/chainId must NOT have more than 20 characters",
        ));
    }
    if chain.is_empty() || !chain.bytes().all(|b| b.is_ascii_digit()) {
        return Err(ApiError::invalid(
            "request/params/chainId must match pattern \"^\\d+$\"",
        ));
    }
    if address.chars().count() < 42 {
        return Err(ApiError::invalid(
            "request/params/address must NOT have fewer than 42 characters",
        ));
    }
    if address.chars().count() > 42 {
        return Err(ApiError::invalid(
            "request/params/address must NOT have more than 42 characters",
        ));
    }
    let normalized = checksum_address(address).ok_or_else(|| {
        ApiError::invalid("request/params/address must match pattern \"(\\b0x[a-fA-F0-9]{40}\\b)\"")
    })?;
    if chain != "1" {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "unsupported_chain",
            format!("Chain {chain} not found"),
        ));
    }
    let hex = &address[2..];
    let mixed_case =
        hex.bytes().any(|b| b.is_ascii_lowercase()) && hex.bytes().any(|b| b.is_ascii_uppercase());
    if mixed_case && address != normalized {
        return Err(ApiError::invalid(format!("Invalid address: {address}")));
    }
    Ok(address.to_ascii_lowercase())
}

pub(crate) fn checksum_address(address: &str) -> Option<String> {
    if address.len() != 42
        || !address.starts_with("0x")
        || !address.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
    {
        return None;
    }
    let lower = address[2..].to_ascii_lowercase();
    let hash = Keccak256::digest(lower.as_bytes());
    let mut result = String::from("0x");
    for (i, byte) in lower.bytes().enumerate() {
        let nibble = if i % 2 == 0 {
            hash[i / 2] >> 4
        } else {
            hash[i / 2] & 15
        };
        result.push(if nibble >= 8 {
            byte.to_ascii_uppercase()
        } else {
            byte
        } as char);
    }
    Some(result)
}
