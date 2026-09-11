use std::path::Path;

use axum::body::Body;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header;
use axum::response::IntoResponse;
use axum::response::Response;
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "web/dist/"]
struct Assets;

pub(super) async fn asset(axum::extract::OriginalUri(uri): axum::extract::OriginalUri) -> Response {
    let path = if uri.path() == "/" {
        "index.html"
    } else {
        uri.path().trim_start_matches('/')
    };
    let Some(file) = Assets::get(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let extension = Path::new(path).extension().and_then(|value| value.to_str());
    response(
        StatusCode::OK,
        mime_for_extension(extension),
        file.data.into_owned(),
    )
}

fn mime_for_extension(extension: Option<&str>) -> &'static str {
    match extension {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("json" | "map") => "application/json; charset=utf-8",
        Some(_) | None => "application/octet-stream",
    }
}

pub(super) fn response(status: StatusCode, mime: &'static str, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; connect-src 'self' ws: wss:; script-src 'self'; \
             style-src 'self'; img-src 'self' data:",
        ),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vite_asset_extensions_have_nosniff_compatible_mime_types() {
        assert_eq!(mime_for_extension(Some("svg")), "image/svg+xml");
        assert_eq!(mime_for_extension(Some("png")), "image/png");
        assert_eq!(mime_for_extension(Some("ico")), "image/x-icon");
        assert_eq!(mime_for_extension(Some("woff2")), "font/woff2");
        assert_eq!(
            mime_for_extension(Some("json")),
            "application/json; charset=utf-8"
        );
        assert_eq!(
            mime_for_extension(Some("map")),
            "application/json; charset=utf-8"
        );
    }
}
