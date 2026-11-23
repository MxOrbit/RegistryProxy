use axum::{
    Json,
    extract::{Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::AppState;

pub(crate) fn expected_header(user: &str, pass: &str) -> Vec<u8> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
    format!("Basic {encoded}").into_bytes()
}

/// Terminating Basic Auth: a valid `Authorization` header is consumed here and never
/// reaches the upstream. `/v2/` is answered directly because forwarding it would
/// return the upstream Bearer challenge and make `docker login` fail.
pub(crate) async fn basic_auth(
    State(state): State<AppState>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(expected) = state.basic_auth.as_deref() else {
        return next.run(req).await;
    };

    let authorized = req
        .headers()
        .get(header::AUTHORIZATION)
        .is_some_and(|value| bool::from(value.as_bytes().ct_eq(expected)));
    if !authorized {
        return (
            StatusCode::UNAUTHORIZED,
            [(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"Registry Proxy\""),
            )],
            Json(json!({
                "errors": [{ "code": "UNAUTHORIZED", "message": "authentication required" }]
            })),
        )
            .into_response();
    }

    if matches!(req.uri().path(), "/v2/" | "/v2") {
        return Json(json!({})).into_response();
    }

    req.headers_mut().remove(header::AUTHORIZATION);
    next.run(req).await
}
