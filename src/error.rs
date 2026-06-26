use axum::{
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("upstream fetch failed: {0}")]
    UpstreamFetch(#[from] reqwest::Error),

    #[error("HLS parse error: {0}")]
    ParseError(String),

    #[error("invalid URL: {0}")]
    InvalidUrl(String),

    #[error("upstream returned non-2xx status {status}: {body}")]
    UpstreamStatus { status: u16, body: String },
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let message = self.to_string();
        let status_code = match &self {
            AppError::UpstreamFetch(_) | AppError::UpstreamStatus { .. } => {
                StatusCode::BAD_GATEWAY
            }
            AppError::ParseError(_) | AppError::InvalidUrl(_) => StatusCode::BAD_REQUEST,
        };
        let mut res = (status_code, message).into_response();
        res.headers_mut().insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        res
    }
}

impl From<anyhow::Error> for AppError {
    fn from(e: anyhow::Error) -> Self {
        AppError::ParseError(e.to_string())
    }
}
