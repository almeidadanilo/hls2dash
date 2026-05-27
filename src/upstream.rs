use crate::error::AppError;
use axum::body::Body;
use bytes::Bytes;
use reqwest::Client;
use tracing::debug;

/// Fetch a remote URL as text, returning (body_bytes, content_type).
pub async fn fetch_text(client: &Client, url: &str) -> Result<(Bytes, String), AppError> {
    debug!(url = %url, "fetching upstream text");
    let response = client.get(url).send().await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| String::from("<unreadable>"));
        return Err(AppError::UpstreamStatus { status, body });
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();

    let body = response.bytes().await?;
    Ok((body, content_type))
}

/// Fetch a remote URL and return a streaming `Body` along with forwarded headers.
pub async fn fetch_stream(
    client: &Client,
    url: &str,
) -> Result<(Body, Option<String>, Option<u64>), AppError> {
    debug!(url = %url, "streaming upstream segment");
    let response = client.get(url).send().await?;

    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| String::from("<unreadable>"));
        return Err(AppError::UpstreamStatus { status, body });
    }

    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    let content_length = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let stream = response.bytes_stream();
    let body = Body::from_stream(stream);

    Ok((body, content_type, content_length))
}
