//! `GET /fsraw?path=/absolute/path` — raw local files for the `file`
//! panel type. Delegates to tower-http's `ServeFile` for MIME detection
//! and HTTP Range support (audio/video seeking in the WebView).
//!
//! Unrestricted local read access is the documented panel-type trust
//! model (spec-gui "Custom panel type trust model").

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tower::util::ServiceExt;
use tower_http::services::ServeFile;

pub async fn serve(request: Request<Body>) -> Response {
    let path = request
        .uri()
        .query()
        .and_then(|query| {
            url_decode(query)
                .split('&')
                .find_map(|pair| pair.strip_prefix("path=").map(str::to_string))
        });
    let Some(path) = path else {
        return (StatusCode::BAD_REQUEST, "missing 'path' query parameter").into_response();
    };
    if !std::path::Path::new(&path).is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }

    match ServeFile::new(&path).oneshot(request).await {
        Ok(response) => response.map(Body::new).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Minimal percent-decoding for the `path` query parameter.
fn url_decode(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let decoded = if bytes[i] == b'%' && i + 2 < bytes.len() {
            std::str::from_utf8(&bytes[i + 1..i + 3])
                .ok()
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        } else {
            None
        };
        match decoded {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_decode() {
        assert_eq!(url_decode("path=/tmp/a%20b.txt"), "path=/tmp/a b.txt");
        assert_eq!(url_decode("path=/tmp/plain.txt"), "path=/tmp/plain.txt");
        assert_eq!(url_decode("a%2Fb"), "a/b");
    }
}
