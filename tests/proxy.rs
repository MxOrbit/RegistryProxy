mod common;

use std::{net::SocketAddr, sync::Arc};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    response::Response,
    routing::any,
};
use common::{Recorded, Recorder, TestProxy, spawn_mock, spawn_proxy, test_config};
use serde_json::{Value, json};

const HUB_HOST: &str = "registry-1.docker.io";
const AUTH_URL: &str = "http://auth.docker.io";
const HUB_CHALLENGE: &str =
    r#"Bearer realm="http://auth.docker.io/token",service="registry.docker.io""#;
const QUAY_CHALLENGE: &str =
    r#"Bearer realm="http://quay.io/token",service="quay.io",scope="registry:catalog:*""#;
const MANIFEST_TYPE: &str = "application/vnd.docker.distribution.manifest.v2+json";
const MANIFEST: &str =
    r#"{"schemaVersion":2,"mediaType":"application/vnd.docker.distribution.manifest.v2+json"}"#;
const DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
const DENIED_BODY: &str =
    r#"{"errors":[{"code":"DENIED","message":"upstream registry not allowed"}]}"#;

fn reply(status: u16, headers: &[(&str, &str)], body: impl Into<Body>) -> Response {
    let mut builder = Response::builder().status(status);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    builder.body(body.into()).unwrap()
}

fn mock_router(handler: impl Fn(&Request) -> Response + Send + Sync + 'static) -> Router {
    let handler = Arc::new(handler);
    Router::new().fallback(move |req: Request| {
        let handler = handler.clone();
        async move { handler(&req) }
    })
}

fn canned(status: u16, headers: &[(&str, &str)], body: impl Into<Vec<u8>>) -> Router {
    let headers: Vec<(String, String)> = headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let body = body.into();
    mock_router(move |_| {
        let pairs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        reply(status, &pairs, body.clone())
    })
}

fn token_router(token: &str) -> Router {
    canned(
        200,
        &[("content-type", "application/json")],
        json!({ "token": token }).to_string(),
    )
}

fn authorization(req: &Request) -> Option<&str> {
    req.headers()
        .get("authorization")
        .and_then(|value| value.to_str().ok())
}

fn quay_registry() -> Router {
    mock_router(|req| {
        let authorized = authorization(req) == Some("Bearer quay-token");
        match req.uri().path() {
            "/token" => reply(
                200,
                &[("content-type", "application/json")],
                r#"{"token":"quay-token"}"#,
            ),
            "/v2/" if authorized => reply(200, &[], "{}"),
            "/v2/" => reply(401, &[("www-authenticate", QUAY_CHALLENGE)], ""),
            path if path.contains("/manifests/") && authorized => reply(
                200,
                &[
                    ("content-type", MANIFEST_TYPE),
                    ("docker-content-digest", DIGEST),
                ],
                MANIFEST,
            ),
            path if path.contains("/manifests/") => {
                reply(401, &[("www-authenticate", QUAY_CHALLENGE)], "")
            }
            _ => reply(404, &[], ""),
        }
    })
}

#[derive(Default)]
struct Mocks {
    hub: Router,
    auth: Router,
    quay: Router,
    cdn: Router,
}

struct World {
    proxy: TestProxy,
    hub: Recorder,
    auth: Recorder,
    quay: Recorder,
    cdn: Recorder,
}

impl World {
    async fn start(vars: &[(&str, &str)], mocks: Mocks) -> World {
        let (hub_addr, hub) = spawn_mock(mocks.hub).await;
        let (auth_addr, auth) = spawn_mock(mocks.auth).await;
        let (quay_addr, quay) = spawn_mock(mocks.quay).await;
        let (cdn_addr, cdn) = spawn_mock(mocks.cdn).await;
        let mut all_vars = vars.to_vec();
        all_vars.push(("AUTH_URL", AUTH_URL));
        let proxy = spawn_proxy(
            test_config(&all_vars),
            &[
                (HUB_HOST, hub_addr),
                ("auth.docker.io", auth_addr),
                ("quay.io", quay_addr),
                ("cdn.test", cdn_addr),
            ],
        )
        .await;
        World {
            proxy,
            hub,
            auth,
            quay,
            cdn,
        }
    }

    async fn hub_only(hub: Router) -> World {
        World::start(
            &[],
            Mocks {
                hub,
                auth: token_router("hub-token"),
                ..Mocks::default()
            },
        )
        .await
    }

    fn upstream_request_count(&self) -> usize {
        self.hub.count() + self.auth.count() + self.quay.count() + self.cdn.count()
    }
}

fn closed_addr() -> SocketAddr {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
}

fn query_pairs(recorded: &Recorded) -> Vec<(String, String)> {
    let query = recorded.uri.query().unwrap_or("");
    form_urlencoded::parse(query.as_bytes())
        .into_owned()
        .collect()
}

fn query_values(recorded: &Recorded, key: &str) -> Vec<String> {
    query_pairs(recorded)
        .into_iter()
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value)
        .collect()
}

fn paths(recorder: &Recorder) -> Vec<String> {
    recorder
        .requests()
        .iter()
        .map(|recorded| recorded.uri.path().to_string())
        .collect()
}

fn header<'a>(response: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn patterned_bytes(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect()
}

#[tokio::test]
async fn hub_manifest_request_adds_library_prefix_and_bearer_token() {
    let hub = canned(200, &[("content-type", MANIFEST_TYPE)], MANIFEST);
    let world = World::hub_only(hub).await;

    let response = world
        .proxy
        .get("/v2/nginx/manifests/latest")
        .header("Authorization", "Bearer client-token")
        .header("Accept", MANIFEST_TYPE)
        .header("X-Amz-Content-Sha256", "client-sha")
        .header("Cookie", "session=1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);

    assert_eq!(world.auth.count(), 1);
    let token_request = world.auth.last();
    assert_eq!(token_request.uri.path(), "/token");
    assert_eq!(
        query_values(&token_request, "scope"),
        ["repository:library/nginx:pull"]
    );
    assert_eq!(
        query_values(&token_request, "service"),
        ["registry.docker.io"]
    );
    assert_eq!(token_request.header("authorization"), None);
    assert_eq!(token_request.header("host"), Some("auth.docker.io"));

    assert_eq!(world.hub.count(), 1);
    let upstream = world.hub.last();
    assert_eq!(upstream.method, reqwest::Method::GET);
    assert_eq!(
        upstream.path_and_query(),
        "/v2/library/nginx/manifests/latest"
    );
    assert_eq!(upstream.header("authorization"), Some("Bearer hub-token"));
    assert_eq!(upstream.header("accept"), Some(MANIFEST_TYPE));
    assert_eq!(upstream.header("x-amz-content-sha256"), Some("client-sha"));
    assert_eq!(upstream.header("cookie"), None);
    assert_eq!(upstream.header("cache-control"), Some("max-age=0"));
    assert_eq!(upstream.header("host"), Some(HUB_HOST));
}

#[tokio::test]
async fn hub_manifest_response_is_passed_through_with_cors_headers() {
    let hub = canned(
        200,
        &[
            ("content-type", MANIFEST_TYPE),
            ("docker-content-digest", DIGEST),
            ("etag", "\"abc\""),
            ("x-custom", "kept"),
        ],
        MANIFEST,
    );
    let world = World::hub_only(hub).await;

    let response = world
        .proxy
        .get("/v2/library/nginx/manifests/latest")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "content-type"), Some(MANIFEST_TYPE));
    assert_eq!(header(&response, "docker-content-digest"), Some(DIGEST));
    assert_eq!(header(&response, "etag"), Some("\"abc\""));
    assert_eq!(header(&response, "x-custom"), Some("kept"));
    assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));
    assert_eq!(
        header(&response, "access-control-expose-headers"),
        Some("*")
    );
    assert_eq!(response.text().await.unwrap(), MANIFEST);
}

#[tokio::test]
async fn hub_error_status_and_body_are_passed_through() {
    let errors = r#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"manifest unknown"}]}"#;
    let hub = canned(404, &[("content-type", "application/json")], errors);
    let world = World::hub_only(hub).await;

    let response = world
        .proxy
        .get("/v2/nginx/manifests/missing")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 404);
    assert_eq!(header(&response, "content-type"), Some("application/json"));
    assert_eq!(response.text().await.unwrap(), errors);
}

#[tokio::test]
async fn hub_library_prefix_only_applies_to_three_segment_paths() {
    let world = World::hub_only(canned(200, &[], "ok")).await;
    let cases = [
        (
            "/v2/nginx/blobs/sha256:abc",
            "/v2/library/nginx/blobs/sha256:abc",
            "library/nginx",
        ),
        (
            "/v2/myorg/app/manifests/v1",
            "/v2/myorg/app/manifests/v1",
            "myorg/app",
        ),
        (
            "/v2/library/nginx/manifests/v1",
            "/v2/library/nginx/manifests/v1",
            "library/nginx",
        ),
        ("/v2/a/b/c/tags/list", "/v2/a/b/c/tags/list", "a/b/c"),
    ];

    for (requested, upstream_path, repo) in cases {
        let response = world.proxy.get(requested).send().await.unwrap();
        assert_eq!(response.status(), 200, "{requested}");
        assert_eq!(
            world.hub.last().path_and_query(),
            upstream_path,
            "{requested}"
        );
        assert_eq!(
            query_values(&world.auth.last(), "scope"),
            [format!("repository:{repo}:pull")],
            "{requested}"
        );
    }
}

#[tokio::test]
async fn hub_token_falls_back_to_access_token_field() {
    let auth = canned(200, &[], r#"{"access_token":"fallback-token"}"#);
    let world = World::start(
        &[],
        Mocks {
            hub: canned(200, &[], "ok"),
            auth,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/nginx/manifests/latest")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        world.hub.last().header("authorization"),
        Some("Bearer fallback-token")
    );
}

#[tokio::test]
async fn hub_request_proceeds_without_authorization_when_token_is_missing() {
    let auth = canned(200, &[], "{}");
    let world = World::start(
        &[],
        Mocks {
            hub: canned(200, &[], "ok"),
            auth,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/nginx/manifests/latest")
        .header("Authorization", "Bearer client-token")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(world.hub.count(), 1);
    assert_eq!(world.hub.last().header("authorization"), None);
}

#[tokio::test]
async fn hub_token_response_that_is_not_json_returns_bad_gateway() {
    let auth = canned(200, &[], "<html>nope</html>");
    let world = World::start(
        &[],
        Mocks {
            hub: canned(200, &[], "ok"),
            auth,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/nginx/manifests/latest")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 502);
    let body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(body["error"], "Bad Gateway");
    assert_eq!(world.hub.count(), 0);
}

#[tokio::test]
async fn hub_token_endpoint_unreachable_returns_bad_gateway() {
    let (hub_addr, hub) = spawn_mock(canned(200, &[], "ok")).await;
    let proxy = spawn_proxy(
        test_config(&[("AUTH_URL", AUTH_URL)]),
        &[(HUB_HOST, hub_addr), ("auth.docker.io", closed_addr())],
    )
    .await;

    let response = proxy
        .get("/v2/nginx/manifests/latest")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 502);
    assert_eq!(hub.count(), 0);
}

#[tokio::test]
async fn hub_blob_absolute_redirect_is_followed_without_authorization() {
    let hub = canned(
        307,
        &[
            ("location", "http://cdn.test/storage/blob?sig=abc"),
            ("docker-content-digest", DIGEST),
        ],
        "",
    );
    let blob = patterned_bytes(1000);
    let cdn = canned(
        200,
        &[
            ("content-type", "application/octet-stream"),
            ("cache-control", "public, max-age=31536000"),
        ],
        blob.clone(),
    );
    let world = World::start(
        &[],
        Mocks {
            hub,
            auth: token_router("hub-token"),
            cdn,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/nginx/blobs/sha256:abc")
        .header("X-Amz-Content-Sha256", "client-sha")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "location"), None);
    assert_eq!(header(&response, "cache-control"), Some("max-age=1500"));
    assert_eq!(
        header(&response, "content-type"),
        Some("application/octet-stream")
    );
    assert_eq!(header(&response, "content-length"), Some("1000"));
    assert_eq!(header(&response, "docker-content-digest"), Some(DIGEST));
    assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));
    assert_eq!(response.bytes().await.unwrap().as_ref(), blob.as_slice());

    assert_eq!(world.hub.count(), 1);
    assert_eq!(
        world.hub.last().header("authorization"),
        Some("Bearer hub-token")
    );
    assert_eq!(world.cdn.count(), 1);
    let cdn_request = world.cdn.last();
    assert_eq!(cdn_request.method, reqwest::Method::GET);
    assert_eq!(cdn_request.path_and_query(), "/storage/blob?sig=abc");
    assert_eq!(cdn_request.header("authorization"), None);
    assert_eq!(cdn_request.header("x-amz-content-sha256"), None);
    assert_eq!(cdn_request.header("host"), Some("cdn.test"));
    assert_eq!(cdn_request.header("user-agent"), Some("Docker-Client"));
}

#[tokio::test]
async fn hub_blob_relative_redirect_is_resolved_against_upstream_url() {
    let hub = mock_router(|req| match req.uri().path() {
        "/storage/blobs/abc" => reply(
            200,
            &[("content-type", "application/octet-stream")],
            "blob-bytes",
        ),
        _ => reply(
            307,
            &[
                ("location", "/storage/blobs/abc?token=1"),
                ("docker-content-digest", DIGEST),
            ],
            "",
        ),
    });
    let world = World::hub_only(hub).await;

    let response = world
        .proxy
        .get("/v2/nginx/blobs/sha256:abc")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "location"), None);
    assert_eq!(header(&response, "cache-control"), Some("max-age=1500"));
    assert_eq!(header(&response, "docker-content-digest"), Some(DIGEST));
    assert_eq!(header(&response, "content-length"), Some("10"));
    assert_eq!(response.text().await.unwrap(), "blob-bytes");

    let requests = world.hub.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].path_and_query(),
        "/v2/library/nginx/blobs/sha256:abc"
    );
    assert_eq!(requests[1].path_and_query(), "/storage/blobs/abc?token=1");
    assert_eq!(requests[1].header("authorization"), None);
}

#[tokio::test]
async fn redirect_adds_missing_docker_headers_but_keeps_final_ones() {
    let hub = canned(
        307,
        &[
            ("location", "http://cdn.test/blob"),
            ("docker-content-digest", DIGEST),
            ("docker-distribution-api-version", "registry/2.0"),
        ],
        "",
    );
    let cdn = canned(
        200,
        &[("docker-distribution-api-version", "cdn/1.0")],
        "payload",
    );
    let world = World::start(
        &[],
        Mocks {
            hub,
            auth: token_router("hub-token"),
            cdn,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/nginx/blobs/sha256:abc")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "docker-content-digest"), Some(DIGEST));
    assert_eq!(
        header(&response, "docker-distribution-api-version"),
        Some("cdn/1.0")
    );
}

#[tokio::test]
async fn redirect_chain_on_the_cdn_is_followed() {
    let hub = canned(307, &[("location", "http://cdn.test/first")], "");
    let cdn = mock_router(|req| match req.uri().path() {
        "/first" => reply(302, &[("location", "/second")], ""),
        _ => reply(200, &[], "final"),
    });
    let world = World::start(
        &[],
        Mocks {
            hub,
            auth: token_router("hub-token"),
            cdn,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/nginx/blobs/sha256:abc")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "final");
    assert_eq!(paths(&world.cdn), ["/first", "/second"]);
}

#[tokio::test]
async fn redirect_on_other_paths_does_not_leak_client_authorization() {
    let hub = canned(302, &[("location", "http://cdn.test/catalog")], "");
    let cdn = canned(200, &[], "from-cdn");
    let world = World::start(
        &[],
        Mocks {
            hub,
            cdn,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/_catalog")
        .header("Authorization", "Bearer client-token")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "from-cdn");
    assert_eq!(
        world.hub.last().header("authorization"),
        Some("Bearer client-token")
    );
    assert_eq!(world.cdn.last().header("authorization"), None);
    assert_eq!(world.auth.count(), 0);
}

#[tokio::test]
async fn hub_token_request_scope_gets_library_prefix() {
    let auth = canned(
        200,
        &[("content-type", "application/json")],
        r#"{"token":"abc"}"#,
    );
    let world = World::start(
        &[],
        Mocks {
            auth,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/token?scope=repository%3Abusybox%3Apull&service=registry.docker.io")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), r#"{"token":"abc"}"#);
    assert_eq!(world.hub.count(), 0);
    assert_eq!(world.auth.count(), 1);
    assert_eq!(
        world.auth.last().path_and_query(),
        "/token?scope=repository%3Alibrary%2Fbusybox%3Apull&service=registry.docker.io"
    );
    assert_eq!(world.auth.last().header("host"), Some("auth.docker.io"));
}

#[tokio::test]
async fn hub_token_request_scope_is_left_alone_when_it_should_not_be_rewritten() {
    let world = World::start(
        &[],
        Mocks {
            auth: canned(200, &[], "{}"),
            ..Mocks::default()
        },
    )
    .await;
    let queries = [
        "scope=repository%3Alibrary%2Fbusybox%3Apull&service=registry.docker.io",
        "scope=repository%3Amyorg%2fapp%3Apull&service=registry.docker.io",
        "service=registry.docker.io",
        "scope=repository%3Abusybox",
    ];

    for query in queries {
        world
            .proxy
            .get(&format!("/token?{query}"))
            .send()
            .await
            .unwrap();
        assert_eq!(
            world.auth.last().path_and_query(),
            format!("/token?{query}")
        );
    }
}

#[tokio::test]
async fn hub_token_request_rewrites_only_the_first_scope_and_keeps_repeated_keys() {
    let world = World::start(
        &[],
        Mocks {
            auth: canned(200, &[], "{}"),
            ..Mocks::default()
        },
    )
    .await;

    world
        .proxy
        .get("/token?scope=repository%3Aa%3Apull&scope=repository%3Ab%3Apull&service=registry.docker.io")
        .send()
        .await
        .unwrap();

    assert_eq!(
        world.auth.last().path_and_query(),
        "/token?scope=repository%3Alibrary%2Fa%3Apull&scope=repository%3Ab%3Apull&service=registry.docker.io"
    );
}

#[tokio::test]
async fn v2_token_path_also_goes_to_auth_url() {
    let world = World::start(
        &[],
        Mocks {
            auth: canned(200, &[], "{}"),
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/token?service=registry.docker.io")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        world.auth.last().path_and_query(),
        "/v2/token?service=registry.docker.io"
    );
    assert_eq!(world.hub.count(), 0);
}

#[tokio::test]
async fn hub_token_request_uses_host_of_configured_auth_url() {
    let (hub_addr, hub) = spawn_mock(canned(200, &[], "{}")).await;
    let (auth_addr, auth) = spawn_mock(canned(200, &[], r#"{"token":"x"}"#)).await;
    let proxy = spawn_proxy(
        test_config(&[("AUTH_URL", "http://auth.example.test")]),
        &[(HUB_HOST, hub_addr), ("auth.example.test", auth_addr)],
    )
    .await;

    let response = proxy
        .get("/token?service=registry.docker.io")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(auth.count(), 1);
    assert_eq!(auth.last().header("host"), Some("auth.example.test"));
    assert_eq!(hub.count(), 0);
}

#[tokio::test]
async fn repository_named_tokenizer_is_not_a_token_request() {
    let world = World::hub_only(canned(200, &[], MANIFEST)).await;

    let response = world
        .proxy
        .get("/v2/tokenizer/manifests/latest")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        world.hub.last().path_and_query(),
        "/v2/library/tokenizer/manifests/latest"
    );
    assert_eq!(
        world.hub.last().header("authorization"),
        Some("Bearer hub-token")
    );
    assert_eq!(
        query_values(&world.auth.last(), "scope"),
        ["repository:library/tokenizer:pull"]
    );
}

#[tokio::test]
async fn non_hub_token_request_goes_to_upstream_host_unchanged() {
    let quay = canned(200, &[], r#"{"token":"quay"}"#);
    let world = World::start(
        &[],
        Mocks {
            quay,
            ..Mocks::default()
        },
    )
    .await;
    let target = "/token?scope=repository%3Aorg%2Fapp%3Apull&scope=repository%3Aother%3Apull&service=quay.io&ns=quay.io";

    let response = world.proxy.get(target).send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), r#"{"token":"quay"}"#);
    assert_eq!(world.quay.count(), 1);
    assert_eq!(world.quay.last().path_and_query(), target);
    assert_eq!(world.quay.last().header("host"), Some("quay.io"));
    assert_eq!(world.auth.count(), 0);
    assert_eq!(world.hub.count(), 0);
}

#[tokio::test]
async fn hub_ping_rewrites_www_authenticate_realm_to_proxy_url() {
    let hub = canned(401, &[("www-authenticate", HUB_CHALLENGE)], "");
    let world = World::start(
        &[],
        Mocks {
            hub,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/")
        .header("Authorization", "Bearer client-token")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 401);
    let expected = format!(
        r#"Bearer realm="{}/token",service="registry.docker.io""#,
        world.proxy.url
    );
    assert_eq!(
        header(&response, "www-authenticate"),
        Some(expected.as_str())
    );
    assert_eq!(world.hub.count(), 1);
    assert_eq!(world.hub.last().path_and_query(), "/v2/");
    assert_eq!(
        world.hub.last().header("authorization"),
        Some("Bearer client-token")
    );
    assert_eq!(world.auth.count(), 0);
}

#[tokio::test]
async fn forwarded_proto_selects_scheme_of_rewritten_realm() {
    let hub = canned(401, &[("www-authenticate", HUB_CHALLENGE)], "");
    let world = World::start(
        &[],
        Mocks {
            hub,
            ..Mocks::default()
        },
    )
    .await;
    let authority = world.proxy.addr;
    let cases = [
        (Some("https"), "https"),
        (Some("HTTPS"), "https"),
        (Some("https, http"), "https"),
        (Some("http"), "http"),
        (Some("ftp"), "http"),
        (None, "http"),
    ];

    for (forwarded, scheme) in cases {
        let mut request = world.proxy.get("/v2/");
        if let Some(value) = forwarded {
            request = request.header("X-Forwarded-Proto", value);
        }
        let response = request.send().await.unwrap();
        let expected =
            format!(r#"Bearer realm="{scheme}://{authority}/token",service="registry.docker.io""#);
        assert_eq!(
            header(&response, "www-authenticate"),
            Some(expected.as_str()),
            "{forwarded:?}"
        );
    }
}

#[tokio::test]
async fn rewritten_realm_uses_the_client_host_header() {
    let hub = canned(401, &[("www-authenticate", HUB_CHALLENGE)], "");
    let world = World::start(
        &[],
        Mocks {
            hub,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/")
        .header("Host", "mirror.example.com")
        .header("X-Forwarded-Proto", "https")
        .send()
        .await
        .unwrap();

    assert_eq!(
        header(&response, "www-authenticate"),
        Some(r#"Bearer realm="https://mirror.example.com/token",service="registry.docker.io""#)
    );
}

#[tokio::test]
async fn route_mode_host_prefix_authenticates_against_challenge_realm() {
    let world = World::start(
        &[("MODE", "route")],
        Mocks {
            quay: quay_registry(),
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/org/app/manifests/latest")
        .header("Host", "quay.example.com")
        .header("Authorization", "Bearer client-token")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "docker-content-digest"), Some(DIGEST));
    assert_eq!(response.text().await.unwrap(), MANIFEST);

    assert_eq!(
        paths(&world.quay),
        ["/v2/", "/token", "/v2/org/app/manifests/latest"]
    );
    let requests = world.quay.requests();
    assert_eq!(requests[0].header("authorization"), None);
    assert_eq!(query_values(&requests[1], "service"), ["quay.io"]);
    assert_eq!(
        query_values(&requests[1], "scope"),
        ["repository:org/app:pull"]
    );
    assert!(
        requests[1]
            .uri
            .query()
            .unwrap()
            .contains("scope=repository%3Aorg%2Fapp%3Apull"),
        "scope should be form-encoded: {:?}",
        requests[1].uri.query()
    );
    assert_eq!(requests[1].header("authorization"), None);
    assert_eq!(
        requests[2].header("authorization"),
        Some("Bearer quay-token")
    );
    assert_eq!(world.hub.count(), 0);
}

#[tokio::test]
async fn ns_query_selects_non_hub_upstream_without_library_prefix() {
    let world = World::start(
        &[],
        Mocks {
            quay: quay_registry(),
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/app/manifests/latest?ns=quay.io")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let requests = world.quay.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(query_values(&requests[1], "scope"), ["repository:app:pull"]);
    assert_eq!(
        requests[2].path_and_query(),
        "/v2/app/manifests/latest?ns=quay.io"
    );
    assert_eq!(
        requests[2].header("authorization"),
        Some("Bearer quay-token")
    );
    assert_eq!(world.auth.count(), 0);
}

#[tokio::test]
async fn non_hub_without_challenge_requests_content_without_authorization() {
    let quay = mock_router(|req| match req.uri().path() {
        "/v2/" => reply(200, &[], "{}"),
        _ => reply(200, &[("content-type", MANIFEST_TYPE)], MANIFEST),
    });
    let world = World::start(
        &[],
        Mocks {
            quay,
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/org/app/manifests/latest?ns=quay.io")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(paths(&world.quay), ["/v2/", "/v2/org/app/manifests/latest"]);
    assert_eq!(world.quay.last().header("authorization"), None);
}

#[tokio::test]
async fn non_hub_ping_challenge_is_answered_with_token_and_retried() {
    let world = World::start(
        &[],
        Mocks {
            quay: quay_registry(),
            ..Mocks::default()
        },
    )
    .await;

    let response = world.proxy.get("/v2/?ns=quay.io").send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "www-authenticate"), None);
    assert_eq!(response.text().await.unwrap(), "{}");
    assert_eq!(paths(&world.quay), ["/v2/", "/token", "/v2/"]);
    let requests = world.quay.requests();
    assert_eq!(requests[0].path_and_query(), "/v2/?ns=quay.io");
    assert_eq!(requests[0].header("authorization"), None);
    assert_eq!(query_values(&requests[1], "service"), ["quay.io"]);
    assert_eq!(query_values(&requests[1], "scope"), ["registry:catalog:*"]);
    assert_eq!(requests[2].path_and_query(), "/v2/?ns=quay.io");
    assert_eq!(
        requests[2].header("authorization"),
        Some("Bearer quay-token")
    );
}

#[tokio::test]
async fn non_hub_retry_that_is_still_unauthorized_rewrites_upstream_realm() {
    let quay = mock_router(|req| match req.uri().path() {
        "/token" => reply(200, &[], r#"{"token":"quay-token"}"#),
        _ => reply(401, &[("www-authenticate", QUAY_CHALLENGE)], ""),
    });
    let world = World::start(
        &[],
        Mocks {
            quay,
            ..Mocks::default()
        },
    )
    .await;

    let response = world.proxy.get("/v2/?ns=quay.io").send().await.unwrap();

    assert_eq!(response.status(), 401);
    let expected = format!(
        r#"Bearer realm="{}/token",service="quay.io",scope="registry:catalog:*""#,
        world.proxy.url
    );
    assert_eq!(
        header(&response, "www-authenticate"),
        Some(expected.as_str())
    );
    assert_eq!(paths(&world.quay), ["/v2/", "/token", "/v2/"]);
}

#[tokio::test]
async fn non_hub_ping_without_obtainable_token_returns_first_response() {
    let quay = mock_router(|req| match req.uri().path() {
        "/token" => reply(500, &[], "boom"),
        _ => reply(401, &[("www-authenticate", QUAY_CHALLENGE)], "denied"),
    });
    let world = World::start(
        &[],
        Mocks {
            quay,
            ..Mocks::default()
        },
    )
    .await;

    let response = world.proxy.get("/v2/?ns=quay.io").send().await.unwrap();

    assert_eq!(response.status(), 401);
    assert_eq!(header(&response, "www-authenticate"), Some(QUAY_CHALLENGE));
    assert_eq!(response.text().await.unwrap(), "denied");
    assert_eq!(paths(&world.quay), ["/v2/", "/token"]);
}

#[tokio::test]
async fn unknown_ns_is_denied_without_contacting_any_upstream() {
    let world = World::start(&[], Mocks::default()).await;
    let targets = [
        "/v2/nginx/manifests/latest?ns=evil.example.com",
        "/v2/?ns=evil.example.com",
        "/token?service=x&ns=evil.example.com",
        "/v2/_catalog?ns=127.0.0.1%3A9",
    ];

    for target in targets {
        let response = world.proxy.get(target).send().await.unwrap();
        assert_eq!(response.status(), 403, "{target}");
        assert!(header(&response, "content-type").is_some_and(|value| value.contains("json")));
        let body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::from_str::<Value>(DENIED_BODY).unwrap(),
            "{target}"
        );
    }
    assert_eq!(world.upstream_request_count(), 0);
}

#[tokio::test]
async fn ns_docker_io_routes_to_hub_even_when_upstream_is_another_registry() {
    let world = World::start(
        &[("UPSTREAM", "quay.io")],
        Mocks {
            hub: canned(200, &[], MANIFEST),
            auth: token_router("hub-token"),
            quay: quay_registry(),
            ..Mocks::default()
        },
    )
    .await;

    let response = world
        .proxy
        .get("/v2/nginx/manifests/latest?ns=docker.io")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        world.hub.last().path_and_query(),
        "/v2/library/nginx/manifests/latest?ns=docker.io"
    );
    assert_eq!(
        world.hub.last().header("authorization"),
        Some("Bearer hub-token")
    );
    assert_eq!(world.quay.count(), 0);
}

#[tokio::test]
async fn single_mode_uses_configured_upstream() {
    let world = World::start(
        &[("UPSTREAM", "quay.io")],
        Mocks {
            quay: canned(200, &[], "catalog"),
            ..Mocks::default()
        },
    )
    .await;

    let response = world.proxy.get("/v2/_catalog").send().await.unwrap();

    assert_eq!(response.text().await.unwrap(), "catalog");
    assert_eq!(world.quay.last().header("host"), Some("quay.io"));
    assert_eq!(world.hub.count(), 0);
}

#[tokio::test]
async fn query_string_is_forwarded_verbatim_including_repeated_keys() {
    let world = World::hub_only(canned(200, &[], "ok")).await;
    let targets = [
        "/v2/_catalog?n=1&n=2&last=a%2Fb&flag&x=%7e&ns=docker.io",
        "/v2/nginx/manifests/latest?a=1&a=2&b=%20c",
        "/v2/nginx/tags/list?n=10&last=1.0",
    ];

    for target in targets {
        world.proxy.get(target).send().await.unwrap();
        let expected = target.replacen("/v2/nginx/", "/v2/library/nginx/", 1);
        assert_eq!(world.hub.last().path_and_query(), expected);
    }
}

#[tokio::test]
async fn non_hub_token_request_keeps_repeated_scope_parameters() {
    let world = World::start(
        &[],
        Mocks {
            quay: canned(200, &[], "{}"),
            ..Mocks::default()
        },
    )
    .await;
    let target = "/token?scope=repository%3Aa%3Apull&scope=repository%3Ab%3Apull&ns=quay.io";

    world.proxy.get(target).send().await.unwrap();

    assert_eq!(world.quay.last().path_and_query(), target);
}

#[tokio::test]
async fn compressed_body_and_headers_are_passed_through_unchanged() {
    let compressed: Vec<u8> = [&[0x1f, 0x8b, 0x08, 0x00][..], &patterned_bytes(300)].concat();
    let length = compressed.len().to_string();
    let hub = canned(
        200,
        &[
            ("content-encoding", "gzip"),
            ("content-type", "application/json"),
            ("content-length", &length),
        ],
        compressed.clone(),
    );
    let world = World::hub_only(hub).await;

    let response = world
        .proxy
        .get("/v2/_catalog")
        .header("Accept-Encoding", "gzip")
        .send()
        .await
        .unwrap();

    assert_eq!(header(&response, "content-encoding"), Some("gzip"));
    assert_eq!(header(&response, "content-length"), Some(length.as_str()));
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        compressed.as_slice()
    );
    assert_eq!(world.hub.last().header("accept-encoding"), Some("gzip"));
}

#[tokio::test]
async fn upstream_request_headers_follow_the_base_header_rules() {
    let world = World::hub_only(canned(200, &[], "ok")).await;

    world
        .proxy
        .get("/v2/_catalog")
        .header("User-Agent", "podman/5.0")
        .header("Accept", "application/json")
        .header("Accept-Language", "de")
        .header("Authorization", "Bearer client-token")
        .header("X-Amz-Content-Sha256", "client-sha")
        .header("Cache-Control", "no-cache")
        .header("Cookie", "session=1")
        .header("Range", "bytes=0-10")
        .header("X-Forwarded-For", "203.0.113.9")
        .send()
        .await
        .unwrap();

    let upstream = world.hub.last();
    assert_eq!(upstream.header("user-agent"), Some("podman/5.0"));
    assert_eq!(upstream.header("accept"), Some("application/json"));
    assert_eq!(upstream.header("accept-language"), Some("de"));
    assert_eq!(
        upstream.header("authorization"),
        Some("Bearer client-token")
    );
    assert_eq!(upstream.header("x-amz-content-sha256"), Some("client-sha"));
    assert_eq!(upstream.header("cache-control"), Some("max-age=0"));
    assert_eq!(upstream.header("cookie"), None);
    assert_eq!(upstream.header("range"), None);
    assert_eq!(upstream.header("x-forwarded-for"), None);
}

#[tokio::test]
async fn upstream_request_headers_use_defaults_when_client_sends_none() {
    let world = World::hub_only(canned(200, &[], "ok")).await;

    world.proxy.get("/v2/_catalog").send().await.unwrap();

    let upstream = world.hub.last();
    assert_eq!(upstream.header("user-agent"), Some("Docker-Client"));
    assert!(upstream.header("accept").is_some());
    assert_eq!(upstream.header("accept-language"), None);
    assert_eq!(upstream.header("accept-encoding"), None);
    assert_eq!(upstream.header("authorization"), None);
}

#[tokio::test]
async fn content_request_without_repository_match_is_forwarded_with_base_headers_only() {
    let world = World::hub_only(canned(200, &[], "ok")).await;

    let response = world
        .proxy
        .get("/v2/blobs/uploads?x=1")
        .header("Authorization", "Bearer client-token")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(world.auth.count(), 0);
    assert_eq!(world.hub.last().path_and_query(), "/v2/blobs/uploads?x=1");
    assert_eq!(world.hub.last().header("authorization"), None);
}

#[tokio::test]
async fn request_body_is_not_forwarded_and_method_is_kept() {
    let hub = Router::new().fallback(any(|body: Bytes| async move { body.len().to_string() }));
    let world = World::hub_only(hub).await;

    let response = world
        .proxy
        .request(reqwest::Method::POST, "/v2/_catalog")
        .body("payload")
        .send()
        .await
        .unwrap();

    assert_eq!(world.hub.last().method, reqwest::Method::POST);
    assert_eq!(response.text().await.unwrap(), "0");
}

#[tokio::test]
async fn hop_by_hop_and_security_policy_headers_are_dropped() {
    let hub = mock_router(|_| {
        let mut response = reply(
            200,
            &[
                ("keep-alive", "timeout=5"),
                ("proxy-connection", "keep-alive"),
                ("te", "trailers"),
                ("trailer", "x-checksum"),
                ("upgrade", "h2c"),
                ("content-security-policy", "default-src 'none'"),
                ("content-security-policy-report-only", "default-src 'none'"),
                ("clear-site-data", "\"cache\""),
                ("x-custom", "kept"),
                ("vary", "Accept"),
            ],
            "body",
        );
        response
            .headers_mut()
            .append("vary", "Accept-Encoding".parse().unwrap());
        response
    });
    let world = World::hub_only(hub).await;

    let response = world.proxy.get("/v2/_catalog").send().await.unwrap();

    for dropped in [
        "keep-alive",
        "proxy-connection",
        "te",
        "trailer",
        "upgrade",
        "content-security-policy",
        "content-security-policy-report-only",
        "clear-site-data",
    ] {
        assert_eq!(header(&response, dropped), None, "{dropped}");
    }
    assert_eq!(header(&response, "x-custom"), Some("kept"));
    assert_eq!(response.headers().get_all("vary").iter().count(), 2);
    assert_eq!(response.text().await.unwrap(), "body");
}

#[tokio::test]
async fn head_request_keeps_content_length_and_has_empty_body() {
    let hub = mock_router(|_| {
        reply(
            200,
            &[("content-length", "100"), ("docker-content-digest", DIGEST)],
            "",
        )
    });
    let world = World::hub_only(hub).await;

    let response = world
        .proxy
        .request(reqwest::Method::HEAD, "/v2/nginx/manifests/latest")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(header(&response, "content-length"), Some("100"));
    assert_eq!(header(&response, "docker-content-digest"), Some(DIGEST));
    assert!(response.bytes().await.unwrap().is_empty());
    let upstream = world.hub.last();
    assert_eq!(upstream.method, reqwest::Method::HEAD);
    assert_eq!(
        upstream.path_and_query(),
        "/v2/library/nginx/manifests/latest"
    );
    assert_eq!(upstream.header("authorization"), Some("Bearer hub-token"));
}

#[tokio::test]
async fn unreachable_upstream_returns_bad_gateway_json() {
    let proxy = spawn_proxy(test_config(&[]), &[(HUB_HOST, closed_addr())]).await;

    let response = proxy.get("/v2/_catalog").send().await.unwrap();

    assert_eq!(response.status(), 502);
    assert!(header(&response, "content-type").is_some_and(|value| value.contains("json")));
    let body: Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(body["error"], "Bad Gateway");
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| !message.is_empty())
    );
}

#[tokio::test]
async fn cors_preflight_is_answered_without_contacting_upstream() {
    let world = World::hub_only(canned(200, &[], "ok")).await;

    let response = world
        .proxy
        .request(reqwest::Method::OPTIONS, "/v2/nginx/manifests/latest")
        .header("Origin", "https://app.example.com")
        .header("Access-Control-Request-Method", "GET")
        .header("Access-Control-Request-Headers", "authorization")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 204);
    assert_eq!(header(&response, "access-control-allow-origin"), Some("*"));
    assert_eq!(
        header(&response, "access-control-allow-methods"),
        Some("GET,POST,PUT,PATCH,TRACE,DELETE,HEAD,OPTIONS")
    );
    assert_eq!(header(&response, "access-control-max-age"), Some("1728000"));
    assert!(response.bytes().await.unwrap().is_empty());
    assert_eq!(world.upstream_request_count(), 0);
}

#[tokio::test]
async fn options_without_request_headers_is_forwarded_upstream() {
    let world = World::hub_only(canned(200, &[], "ok")).await;

    let response = world
        .proxy
        .request(reqwest::Method::OPTIONS, "/v2/_catalog")
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(world.hub.count(), 1);
    assert_eq!(world.hub.last().method, reqwest::Method::OPTIONS);
}

#[tokio::test]
async fn large_streamed_body_arrives_byte_identical() {
    let payload = patterned_bytes(8 * 1024 * 1024);
    let hub = canned(
        200,
        &[("content-type", "application/octet-stream")],
        payload.clone(),
    );
    let world = World::hub_only(hub).await;

    let response = world.proxy.get("/v2/_catalog").send().await.unwrap();

    assert_eq!(response.status(), 200);
    assert_eq!(
        header(&response, "content-length"),
        Some(payload.len().to_string().as_str())
    );
    let received = response.bytes().await.unwrap();
    assert_eq!(received.len(), payload.len());
    assert!(
        received.as_ref() == payload.as_slice(),
        "body differs from upstream payload"
    );
}

#[tokio::test]
async fn repeated_accept_headers_are_all_forwarded() {
    let world = World::start(
        &[("MODE", "route")],
        Mocks {
            quay: quay_registry(),
            ..Mocks::default()
        },
    )
    .await;
    let index_type = "application/vnd.oci.image.index.v1+json";

    let response = world
        .proxy
        .get("/v2/org/app/manifests/latest")
        .header("Host", "quay.example.com")
        .header("Accept", MANIFEST_TYPE)
        .header("Accept", index_type)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), 200);
    let manifest_request = world
        .quay
        .requests()
        .into_iter()
        .find(|recorded| recorded.uri.path() == "/v2/org/app/manifests/latest")
        .expect("manifest request");
    let accept: Vec<_> = manifest_request
        .headers
        .get_all("accept")
        .iter()
        .map(|value| value.to_str().unwrap())
        .collect();
    assert_eq!(accept, [MANIFEST_TYPE, index_type]);
}

#[tokio::test]
async fn internal_token_requests_do_not_ask_for_compression() {
    let world = World::start(
        &[("MODE", "route")],
        Mocks {
            hub: canned(200, &[], MANIFEST),
            auth: token_router("hub-token"),
            quay: quay_registry(),
            ..Mocks::default()
        },
    )
    .await;

    for (path, host) in [
        ("/v2/nginx/manifests/latest", "docker.example.com"),
        ("/v2/org/app/manifests/latest", "quay.example.com"),
    ] {
        let response = world
            .proxy
            .get(path)
            .header("Host", host)
            .header("Accept-Encoding", "gzip")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "{path}");
    }

    assert_eq!(world.auth.last().header("accept-encoding"), None);
    assert_eq!(world.hub.last().header("accept-encoding"), Some("gzip"));
    for recorded in world.quay.requests() {
        let expected = recorded
            .uri
            .path()
            .contains("/manifests/")
            .then_some("gzip");
        assert_eq!(
            recorded.header("accept-encoding"),
            expected,
            "{}",
            recorded.path_and_query()
        );
    }
}

#[tokio::test]
async fn non_canonical_paths_are_rejected_without_contacting_upstream() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let world = World::hub_only(canned(200, &[], "ok")).await;

    for path in [
        "/v2/library/nginx/manifests/../../../../v2/_catalog",
        "/v2/library/nginx/manifests/%2e%2e/x",
        "/v2/foo\\bar/manifests/latest",
    ] {
        let mut stream = tokio::net::TcpStream::connect(world.proxy.addr)
            .await
            .unwrap();
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: mirror.test\r\nConnection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut raw = String::new();
        stream.read_to_string(&mut raw).await.unwrap();
        assert!(raw.starts_with("HTTP/1.1 400"), "{path}: {raw}");
        assert!(raw.contains(r#""code":"UNSUPPORTED""#), "{path}: {raw}");
    }
    assert_eq!(world.upstream_request_count(), 0);
}
