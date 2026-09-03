//! `GET /fsraw?path=/absolute/path` — raw local files for the `file`
//! panel type. Delegates to tower-http's `ServeFile` for MIME detection
//! and HTTP Range support (audio/video seeking in the WebView).
//!
//! Unrestricted local read access is the documented panel-type trust
//! model (spec-gui "Custom panel type trust model").
//!
//! # Invariant: a raw file is media, never a document
//!
//! What this endpoint serves is **untrusted content**, and it is only safe as
//! long as it is loaded as *data*: an `<img>`, `<video>` or `<audio>` source.
//! In that position even an SVG carrying a `<script>` does not execute, and a
//! crafted file gets no further than the media decoder (itself confined — see
//! `crate::sandbox` for the helper processes, and `WEBKIT_FORCE_SANDBOX` for
//! the web process).
//!
//! Load the same URL as a **document** — an `<iframe>`, `<object>`, `<embed>`,
//! or a navigation — and the file becomes *code* running in this server's
//! origin. Because the session token travels in the URL here (`?token=…`: an
//! `<img>` `src` cannot carry an `Authorization` header), such a document could
//! read `location.search`, lift the token, and drive the whole `/gui/*`
//! scripting API — `POST /gui/command`, hence the `!` shell commands. A file
//! preview would be a full compromise.
//!
//! No panel may therefore load an `/fsraw` URL as a document; the shipped
//! panels are checked by `tests/panel_invariants.rs`. Should one ever need to,
//! the token must stop travelling in the query string first.

use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use tower::util::ServiceExt;
use tower_http::services::ServeFile;

pub async fn serve(request: Request<Body>) -> Response {
    let path = request.uri().query().and_then(path_param);
    let Some(path) = path else {
        return (StatusCode::BAD_REQUEST, "missing 'path' query parameter").into_response();
    };
    // The `file` panel hands back the escaped handle it was given, so a name no
    // text can represent still reaches the disk exactly (spec-data-model
    // "Tree names").
    let path = crate::fs_path::from_handle(&path);
    if !path.is_file() {
        return StatusCode::NOT_FOUND.into_response();
    }

    match ServeFile::new(&path).oneshot(request).await {
        Ok(response) => response.map(Body::new).into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

/// The `path` query parameter, percent-decoded.
///
/// The query is split into pairs **before** decoding: a file name may itself
/// contain a `&` (percent-encoded as `%26`), and decoding the whole string
/// first would then cut the path short at that character.
fn path_param(query: &str) -> Option<String> {
    query.split('&').find_map(|pair| pair.strip_prefix("path=")).map(url_decode)
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

    #[test]
    fn test_path_param_keeps_an_ampersand_in_the_file_name() {
        // A file name containing "&" arrives percent-encoded (%26). Decoding
        // the whole query string before splitting on "&" would cut the path in
        // half — the file would 404 and the panel would report the media as
        // unplayable.
        assert_eq!(
            path_param("path=%2Ftmp%2Fa%20%26%20b.mp4&token=deadbeef").as_deref(),
            Some("/tmp/a & b.mp4"),
        );
    }

    #[test]
    fn test_path_param_reads_the_parameter_wherever_it_sits() {
        assert_eq!(path_param("path=%2Ftmp%2Fa.txt").as_deref(), Some("/tmp/a.txt"));
        assert_eq!(path_param("token=abc&path=%2Ftmp%2Fa.txt").as_deref(), Some("/tmp/a.txt"));
        assert_eq!(path_param("token=abc").as_deref(), None);
        // "pathological=x" must not be mistaken for the "path" parameter.
        assert_eq!(path_param("pathological=x").as_deref(), None);
    }

    #[test]
    fn test_url_decode_escape_at_end_of_string() {
        // A %XX whose last hex digit is the final byte must still decode:
        // `i + 2 < len` is exactly `i + 3 <= len`, the bound for `[i+1..i+3]`.
        assert_eq!(url_decode("a%2F"), "a/");
        assert_eq!(url_decode("%2F"), "/");
        assert_eq!(url_decode("dir%20"), "dir ");
        // A truncated escape at the end stays literal (cannot decode).
        assert_eq!(url_decode("a%2"), "a%2");
        assert_eq!(url_decode("a%"), "a%");
    }
}
