use axum::{
    body::Body,
    http::{header::CONTENT_TYPE, HeaderValue, Response, StatusCode},
    response::Html,
};

const INDEX_HTML: &str = include_str!("../ui/index.html");
const APP_CSS: &str = include_str!("../ui/app.css");
const APP_JS: &str = include_str!("../ui/app.js");
const ACCOUNT_CONTROL_CSS: &str = include_str!("../ui/account-control.css");
const ACCOUNT_CONTROL_JS: &str = include_str!("../ui/account-control.js");
const ACCOUNT_INTELLIGENCE_CSS: &str = include_str!("../ui/account-intelligence.css");
const ACCOUNT_INTELLIGENCE_JS: &str = include_str!("../ui/account-intelligence.js");
const BROWSER_CONTROL_CSS: &str = include_str!("../ui/browser-control.css");
const BROWSER_CONTROL_JS: &str = include_str!("../ui/browser-control.js");
const TRACE_CONSOLE_CSS: &str = include_str!("../ui/trace-console.css");
const TRACE_CONSOLE_JS: &str = include_str!("../ui/trace-console.js");

pub async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

pub async fn app_css() -> Response<Body> {
    asset_response("text/css; charset=utf-8", APP_CSS)
}

pub async fn app_js() -> Response<Body> {
    asset_response("text/javascript; charset=utf-8", APP_JS)
}

pub async fn account_control_css() -> Response<Body> {
    asset_response("text/css; charset=utf-8", ACCOUNT_CONTROL_CSS)
}

pub async fn account_control_js() -> Response<Body> {
    asset_response("text/javascript; charset=utf-8", ACCOUNT_CONTROL_JS)
}

pub async fn account_intelligence_css() -> Response<Body> {
    asset_response("text/css; charset=utf-8", ACCOUNT_INTELLIGENCE_CSS)
}

pub async fn account_intelligence_js() -> Response<Body> {
    asset_response("text/javascript; charset=utf-8", ACCOUNT_INTELLIGENCE_JS)
}

pub async fn browser_control_css() -> Response<Body> {
    asset_response("text/css; charset=utf-8", BROWSER_CONTROL_CSS)
}

pub async fn browser_control_js() -> Response<Body> {
    asset_response("text/javascript; charset=utf-8", BROWSER_CONTROL_JS)
}

pub async fn trace_console_css() -> Response<Body> {
    asset_response("text/css; charset=utf-8", TRACE_CONSOLE_CSS)
}

pub async fn trace_console_js() -> Response<Body> {
    asset_response("text/javascript; charset=utf-8", TRACE_CONSOLE_JS)
}

fn asset_response(content_type: &str, content: &'static str) -> Response<Body> {
    let mut response = Response::builder()
        .status(StatusCode::OK)
        .body(Body::from(content))
        .expect("valid UI response");
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str(content_type).expect("valid UI content type"),
    );
    response
}
