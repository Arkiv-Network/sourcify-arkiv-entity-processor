use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;
use std::{error::Error, fmt};

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: &'static str,
}
impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }
    pub fn invalid(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }
    pub fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "backend_unavailable",
            "The configured dataset is unavailable",
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
        use arkiv_entity_processor::bundle::BundleError;
        let mapped = match error.downcast_ref::<BundleError>() {
            Some(BundleError::Unavailable(_)) => Self::unavailable(),
            Some(BundleError::MissingDependency(_)) => Self::new(
                StatusCode::BAD_GATEWAY,
                "missing_dependency",
                "A required stored dependency is missing",
            ),
            Some(BundleError::Ambiguous(_)) => Self::new(
                StatusCode::BAD_GATEWAY,
                "ambiguous_record",
                "Multiple stored records match this lookup",
            ),
            Some(BundleError::UnsupportedSchema(_)) => Self::new(
                StatusCode::BAD_GATEWAY,
                "unsupported_schema",
                "The stored schema is unsupported",
            ),
            _ => Self::corrupt(),
        };
        tracing::warn!(code = mapped.code, error = %error, "contract read failed");
        mapped
    }
}
impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}
impl Error for ApiError {}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": {"code": self.code, "message": self.message}})),
        )
            .into_response()
    }
}

pub fn validate_path(chain: &str, address: &str) -> Result<String, ApiError> {
    if chain != "1" {
        return Err(ApiError::invalid(
            "Only Ethereum Mainnet chainId 1 is supported",
        ));
    }
    if address.len() != 42
        || !address.starts_with("0x")
        || !address.as_bytes()[2..].iter().all(u8::is_ascii_hexdigit)
    {
        return Err(ApiError::invalid(
            "Address must be 0x followed by 40 hexadecimal digits",
        ));
    }
    Ok(address.to_ascii_lowercase())
}
