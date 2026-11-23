mod common;

use axum::{Router, http::StatusCode, routing::any};
use common::{spawn_mock, spawn_proxy, test_config};
use reqwest::{Method, header};

const BASIC_ENV: &[(&str, &str)] = &[
    ("AUTH_MODE", "basic"),
    ("AUTH_USER", "alice"),
    ("AUTH_PASS", "s3cret"),
    ("UPSTREAM", "registry.test"),
];

async fn upstream() -> (std::net::SocketAddr, common::Recorder) {
    spawn_mock(Router::new().fallback(any(|| async { "upstream" }))).await
}

#[tokio::test]
async fn healthz_answers_get_and_head_without_auth_or_ua_checks() {
    let (addr, recorder) = upstream().await;
    let proxy = spawn_proxy(test_config(BASIC_ENV), &[("registry.test", addr)]).await;

    let response = proxy
        .get("/healthz")
        .header(header::USER_AGENT, "Netcraft scanner")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok");

    let response = proxy
        .request(Method::HEAD, "/healthz")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(recorder.count(), 0);
}

#[tokio::test]
async fn basic_auth_rejects_missing_or_wrong_credentials() {
    let (addr, recorder) = upstream().await;
    let proxy = spawn_proxy(test_config(BASIC_ENV), &[("registry.test", addr)]).await;

    for request in [
        proxy.get("/v2/"),
        proxy
            .get("/v2/library/nginx/manifests/latest")
            .basic_auth("alice", Some("wrong")),
        proxy
            .get("/v2/")
            .header(header::AUTHORIZATION, "Bearer token"),
    ] {
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers()[header::WWW_AUTHENTICATE],
            "Basic realm=\"Registry Proxy\""
        );
        assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
        let body: serde_json::Value = response.json_body().await;
        assert_eq!(body["errors"][0]["code"], "UNAUTHORIZED");
        assert_eq!(body["errors"][0]["message"], "authentication required");
    }
    assert_eq!(recorder.count(), 0);
}

#[tokio::test]
async fn basic_auth_answers_v2_ping_itself() {
    let (addr, recorder) = upstream().await;
    let proxy = spawn_proxy(test_config(BASIC_ENV), &[("registry.test", addr)]).await;

    for path in ["/v2/", "/v2"] {
        let response = proxy
            .get(path)
            .basic_auth("alice", Some("s3cret"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body: serde_json::Value = response.json_body().await;
        assert_eq!(body, serde_json::json!({}));
    }
    assert_eq!(recorder.count(), 0);
}

#[tokio::test]
async fn basic_auth_strips_credentials_before_proxying() {
    let (addr, recorder) = upstream().await;
    let proxy = spawn_proxy(test_config(BASIC_ENV), &[("registry.test", addr)]).await;

    let response = proxy
        .get("/v2/_catalog")
        .basic_auth("alice", Some("s3cret"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(recorder.count(), 1);
    assert_eq!(recorder.last().header("authorization"), None);
}

#[tokio::test]
async fn blocked_user_agents_get_404_without_upstream_request() {
    let (addr, recorder) = upstream().await;
    let proxy = spawn_proxy(
        test_config(&[
            ("UPSTREAM", "registry.test"),
            ("BLOCKED_UA", "badbot, crawler"),
        ]),
        &[("registry.test", addr)],
    )
    .await;

    for user_agent in ["Mozilla/5.0 BadBot/1.0", "some-CRAWLER"] {
        let response = proxy
            .get("/v2/")
            .header(header::USER_AGENT, user_agent)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{user_agent}");
        assert!(response.bytes().await.unwrap().is_empty());
    }
    assert_eq!(recorder.count(), 0);
}

trait JsonBody {
    async fn json_body(self) -> serde_json::Value;
}

impl JsonBody for reqwest::Response {
    async fn json_body(self) -> serde_json::Value {
        serde_json::from_slice(&self.bytes().await.unwrap()).unwrap()
    }
}
