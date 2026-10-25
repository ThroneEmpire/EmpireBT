//! The browser side of the web UI, built into the binary: no files to
//! install, and nothing served from disk.

use axum::http::header;
use axum::response::{IntoResponse, Response};

const INDEX: &str = include_str!("../assets/index.html");
const SCRIPT: &str = include_str!("../assets/app.js");
const STYLE: &str = include_str!("../assets/style.css");
const ICON: &str = include_str!("../assets/icon.svg");

fn file(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, content_type),
            // Revalidated on every load, so an upgrade shows at once.
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub(crate) async fn index() -> Response {
    file("text/html; charset=utf-8", INDEX)
}

pub(crate) async fn script() -> Response {
    file("text/javascript; charset=utf-8", SCRIPT)
}

pub(crate) async fn style() -> Response {
    file("text/css; charset=utf-8", STYLE)
}

pub(crate) async fn icon() -> Response {
    file("image/svg+xml", ICON)
}
