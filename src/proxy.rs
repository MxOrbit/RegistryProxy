mod cached;
mod response;
mod token;

use std::borrow::Cow;

use axum::{
    Json,
    extract::{Request, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
        header::{
            ACCEPT, ACCEPT_ENCODING, ACCEPT_LANGUAGE, ACCESS_CONTROL_ALLOW_METHODS,
            ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_HEADERS,
            AUTHORIZATION, CACHE_CONTROL, HOST, RANGE, USER_AGENT, WWW_AUTHENTICATE,
        },
        request::Parts,
    },
    response::{IntoResponse, Response},
};
use reqwest::Url;
use serde_json::json;
use tracing::warn;

use crate::{AppState, cache::is_digest_hex, config::DOCKER_HUB};

const X_AMZ_CONTENT_SHA256: &str = "x-amz-content-sha256";
const CONTENT_KINDS: [&str; 3] = ["/manifests/", "/blobs/", "/tags/"];

pub(crate) async fn handle(State(state): State<AppState>, req: Request) -> Response {
    let (parts, _body) = req.into_parts();
    if is_preflight(&parts) {
        return preflight_response();
    }

    let host = request_host(&parts);
    let query = parts.uri.query();
    let Ok(upstream) = state.config.resolve_upstream(query, Some(&host)) else {
        return denied_response();
    };

    let proxy = Proxy::new(&state, &parts, upstream, &host);
    match proxy.forward(parts.uri.path(), query).await {
        Ok(response) => response,
        Err(UpstreamError::InvalidTarget) => invalid_target_response(),
        Err(UpstreamError::Failed(message)) => {
            warn!(
                method = %parts.method,
                path = parts.uri.path(),
                upstream = proxy.upstream,
                "upstream request failed: {message}"
            );
            bad_gateway_response(message)
        }
    }
}

#[derive(Debug)]
enum UpstreamError {
    /// The request path or query would be rewritten by URL parsing; answered with `400`.
    InvalidTarget,
    /// A failure that happened before the client response started; answered with `502`.
    Failed(String),
}

impl<E: std::error::Error> From<E> for UpstreamError {
    fn from(err: E) -> Self {
        let mut message = err.to_string();
        let mut source = err.source();
        while let Some(cause) = source {
            message.push_str(": ");
            message.push_str(&cause.to_string());
            source = cause.source();
        }
        UpstreamError::Failed(message)
    }
}

struct Proxy<'a> {
    state: &'a AppState,
    method: Method,
    upstream: String,
    hub: bool,
    base_headers: HeaderMap,
    authorization: Option<HeaderValue>,
    amz_content_sha256: Option<HeaderValue>,
    range: bool,
    self_url: String,
}

impl<'a> Proxy<'a> {
    fn new(state: &'a AppState, parts: &Parts, upstream: String, host: &str) -> Self {
        Proxy {
            state,
            method: parts.method.clone(),
            hub: upstream.eq_ignore_ascii_case(DOCKER_HUB),
            upstream,
            base_headers: base_headers(&parts.headers),
            authorization: parts.headers.get(AUTHORIZATION).cloned(),
            amz_content_sha256: parts.headers.get(X_AMZ_CONTENT_SHA256).cloned(),
            range: parts.headers.contains_key(RANGE),
            self_url: self_url(&parts.headers, host),
        }
    }

    async fn forward(&self, path: &str, query: Option<&str>) -> Result<Response, UpstreamError> {
        if is_token_path(path) {
            return self.token_request(path, query).await;
        }
        let path = if self.hub {
            with_library_prefix(path)
        } else {
            Cow::Borrowed(path)
        };
        if is_content_path(&path) {
            return self.content_request(&path, query).await;
        }
        self.passthrough_request(&path, query).await
    }

    async fn token_request(
        &self,
        path: &str,
        query: Option<&str>,
    ) -> Result<Response, UpstreamError> {
        let (origin, query) = if self.hub {
            (self.state.config.auth_url.clone(), query.map(fix_hub_scope))
        } else {
            (self.registry_origin(), query.map(Cow::Borrowed))
        };
        let url = upstream_url(&origin, path, query.as_deref())?;
        let upstream = self
            .send(
                &self.state.client,
                Method::GET,
                url,
                self.base_headers.clone(),
            )
            .await?;
        self.respond(upstream, &origin, false).await
    }

    async fn content_request(
        &self,
        path: &str,
        query: Option<&str>,
    ) -> Result<Response, UpstreamError> {
        if let Some(cache) = &self.state.cache
            && (self.method == Method::GET || self.method == Method::HEAD)
            && !self.range
            && is_canonical_target(path, query)
            && is_routing_only(query)
            && let Some(hex) = blob_digest(path)
        {
            return self.cached_blob(cache, hex, path, query).await;
        }
        let (status, headers, upstream) = self.fetch_content(path, query).await?;
        Ok(self.client_response(status, headers, upstream))
    }

    /// Requests manifests, blobs and tags with an anonymous pull token, following redirects.
    async fn fetch_content(
        &self,
        path: &str,
        query: Option<&str>,
    ) -> Result<(StatusCode, HeaderMap, reqwest::Response), UpstreamError> {
        let url = self.registry_url(path, query)?;
        let Some(repo) = repository(path) else {
            let upstream = self
                .send(
                    &self.state.client,
                    self.method.clone(),
                    url,
                    self.base_headers.clone(),
                )
                .await?;
            return self
                .prepare(upstream, &self.state.config.auth_url, true)
                .await;
        };

        let token = if self.hub {
            self.hub_token(repo).await?
        } else {
            self.registry_token(repo).await?
        };
        let headers = self.forwarded_headers(token);
        let upstream = self
            .send(&self.state.client, self.method.clone(), url, headers)
            .await?;
        self.prepare(upstream, &self.state.config.auth_url, true)
            .await
    }

    async fn passthrough_request(
        &self,
        path: &str,
        query: Option<&str>,
    ) -> Result<Response, UpstreamError> {
        let url = self.registry_url(path, query)?;
        let mut headers = self.forwarded_headers(self.authorization.clone());
        let first = self
            .send(
                &self.state.client,
                self.method.clone(),
                url.clone(),
                headers.clone(),
            )
            .await?;

        if !self.hub
            && first.status() == StatusCode::UNAUTHORIZED
            && let Some(challenge) = first.headers().get(WWW_AUTHENTICATE).cloned()
            && let Some(token) = self.challenge_token(&challenge, None).await
        {
            headers.insert(AUTHORIZATION, token);
            let retried = self
                .send(&self.state.client, self.method.clone(), url, headers)
                .await?;
            return self.respond(retried, &self.registry_origin(), true).await;
        }
        self.respond(first, &self.state.config.auth_url, true).await
    }

    async fn send(
        &self,
        client: &reqwest::Client,
        method: Method,
        url: Url,
        headers: HeaderMap,
    ) -> reqwest::Result<reqwest::Response> {
        client.request(method, url).headers(headers).send().await
    }

    fn registry_origin(&self) -> String {
        format!("{}://{}", self.state.config.upstream_scheme, self.upstream)
    }

    fn registry_url(&self, path: &str, query: Option<&str>) -> Result<Url, UpstreamError> {
        upstream_url(&self.registry_origin(), path, query)
    }

    /// Base headers plus the given `Authorization` and the client's `X-Amz-Content-Sha256`.
    fn forwarded_headers(&self, authorization: Option<HeaderValue>) -> HeaderMap {
        let mut headers = self.base_headers.clone();
        if let Some(value) = authorization {
            headers.insert(AUTHORIZATION, value);
        }
        if let Some(value) = &self.amz_content_sha256 {
            headers.insert(HeaderName::from_static(X_AMZ_CONTENT_SHA256), value.clone());
        }
        headers
    }
}

fn is_preflight(parts: &Parts) -> bool {
    parts.method == Method::OPTIONS && parts.headers.contains_key(ACCESS_CONTROL_REQUEST_HEADERS)
}

fn preflight_response() -> Response {
    (
        StatusCode::NO_CONTENT,
        [
            (ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (
                ACCESS_CONTROL_ALLOW_METHODS,
                "GET,POST,PUT,PATCH,TRACE,DELETE,HEAD,OPTIONS",
            ),
            (ACCESS_CONTROL_MAX_AGE, "1728000"),
        ],
    )
        .into_response()
}

fn denied_response() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "errors": [{ "code": "DENIED", "message": "upstream registry not allowed" }]
        })),
    )
        .into_response()
}

fn invalid_target_response() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "errors": [{ "code": "UNSUPPORTED", "message": "request path or query is not canonical" }]
        })),
    )
        .into_response()
}

fn bad_gateway_response(message: String) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(json!({ "error": "Bad Gateway", "message": message })),
    )
        .into_response()
}

/// Raw `Host` header, else the URI authority, else empty.
fn request_host(parts: &Parts) -> String {
    match parts.headers.get(HOST) {
        Some(value) => String::from_utf8_lossy(value.as_bytes()).into_owned(),
        None => parts
            .uri
            .authority()
            .map(|authority| authority.as_str().to_string())
            .unwrap_or_default(),
    }
}

fn self_url(headers: &HeaderMap, host: &str) -> String {
    let https = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"));
    let proto = if https { "https" } else { "http" };
    format!("{proto}://{host}")
}

fn base_headers(client: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let user_agent = client
        .get(USER_AGENT)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("Docker-Client"));
    headers.insert(USER_AGENT, user_agent);
    for name in [ACCEPT, ACCEPT_LANGUAGE, ACCEPT_ENCODING] {
        for value in client.get_all(&name) {
            headers.append(name.clone(), value.clone());
        }
    }
    if !headers.contains_key(ACCEPT) {
        headers.insert(ACCEPT, HeaderValue::from_static("*/*"));
    }
    headers.insert(CACHE_CONTROL, HeaderValue::from_static("max-age=0"));
    headers
}

/// Builds `{origin}{path}{?query}`, rejecting targets the URL parser would rewrite
/// (dot segments, backslashes, characters it percent-encodes) so that routing
/// decisions made on the raw path always match what is sent upstream.
fn upstream_url(origin: &str, path: &str, query: Option<&str>) -> Result<Url, UpstreamError> {
    if !is_canonical_target(path, query) {
        return Err(UpstreamError::InvalidTarget);
    }
    let suffix = query.map(|query| format!("?{query}")).unwrap_or_default();
    Ok(Url::parse(&format!("{origin}{path}{suffix}"))?)
}

fn is_canonical_target(path: &str, query: Option<&str>) -> bool {
    let suffix = query.map(|query| format!("?{query}")).unwrap_or_default();
    Url::parse(&format!("http://registry.invalid{path}{suffix}"))
        .is_ok_and(|url| url.path() == path && url.query() == query)
}

fn is_token_path(path: &str) -> bool {
    path.rsplit('/').next() == Some("token")
}

/// Rewrites `repository%3Abusybox%3Apull&...` to `repository%3Alibrary%2Fbusybox%3Apull&...`
/// for single-segment Docker Hub repositories.
fn fix_hub_scope(query: &str) -> Cow<'_, str> {
    let lower = query.to_ascii_lowercase();
    if lower.contains("%2f") {
        return Cow::Borrowed(query);
    }
    match lower.find("%3a") {
        Some(index) if query[index + 3..].contains('&') => Cow::Owned(format!(
            "{}%3Alibrary%2F{}",
            &query[..index],
            &query[index + 3..]
        )),
        _ => Cow::Borrowed(query),
    }
}

fn with_library_prefix(path: &str) -> Cow<'_, str> {
    match path.strip_prefix("/v2/") {
        Some(rest)
            if !path.starts_with("/v2/library/")
                && rest.split('/').count() == 3
                && rest.split('/').all(|segment| !segment.is_empty()) =>
        {
            Cow::Owned(format!("/v2/library/{rest}"))
        }
        _ => Cow::Borrowed(path),
    }
}

fn is_content_path(path: &str) -> bool {
    path.starts_with("/v2/") && CONTENT_KINDS.iter().any(|kind| path.contains(kind))
}

/// Shortest match of `^/v2/(.+?)/(manifests|blobs|tags)/`.
fn repository(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/v2/")?;
    rest.match_indices('/')
        .map(|(index, _)| index)
        .filter(|&index| index > 0)
        .find(|&index| {
            CONTENT_KINDS
                .iter()
                .any(|kind| rest[index..].starts_with(kind))
        })
        .map(|index| &rest[..index])
}

/// The hex digest of `/v2/<repo>/blobs/sha256:<64 lowercase hex>`.
fn blob_digest(path: &str) -> Option<&str> {
    let repo = repository(path)?;
    let hex = path
        .strip_prefix("/v2/")?
        .strip_prefix(repo)?
        .strip_prefix("/blobs/sha256:")?;
    is_digest_hex(hex).then_some(hex)
}

/// Whether the query only carries the proxy's own routing keys. Anything else is forwarded
/// upstream and may authorize the request (e.g. signed URLs), so it must not fill a shared cache.
/// `;` is rejected because some upstreams split on it where `form_urlencoded` does not.
fn is_routing_only(query: Option<&str>) -> bool {
    let query = query.unwrap_or("");
    !query.contains(';')
        && form_urlencoded::parse(query.as_bytes()).all(|(key, _)| key == "ns" || key == "hubhost")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_routing_only_queries() {
        for query in [
            None,
            Some(""),
            Some("ns=docker.io"),
            Some("ns=quay.io&hubhost=x"),
        ] {
            assert!(is_routing_only(query), "{query:?}");
        }
        for query in [
            "ns=quay.io&sig=abc",
            "X-Amz-Signature=abc",
            "token",
            "hubhost=x;sig=abc",
        ] {
            assert!(!is_routing_only(Some(query)), "{query}");
        }
    }

    #[test]
    fn extracts_blob_digests() {
        let hex = "0123456789abcdef".repeat(4);
        assert_eq!(
            blob_digest(&format!("/v2/library/nginx/blobs/sha256:{hex}")),
            Some(hex.as_str())
        );
        assert_eq!(
            blob_digest(&format!("/v2/a/b/c/blobs/sha256:{hex}")),
            Some(hex.as_str())
        );
        for path in [
            format!("/v2/nginx/blobs/sha256:{}", hex.to_uppercase()),
            format!("/v2/nginx/blobs/sha256:{hex}0"),
            format!("/v2/nginx/blobs/sha512:{hex}"),
            format!("/v2/nginx/blobs/sha256:{hex}/x"),
            format!("/v2/nginx/manifests/sha256:{hex}"),
            format!("/v2/nginx/manifests/x/blobs/sha256:{hex}"),
            format!("/v2/blobs/sha256:{hex}"),
            "/v2/nginx/blobs/uploads/".to_string(),
        ] {
            assert_eq!(blob_digest(&path), None, "{path}");
        }
    }

    #[test]
    fn detects_token_paths_by_last_segment() {
        assert!(is_token_path("/token"));
        assert!(is_token_path("/v2/token"));
        assert!(!is_token_path("/v2/tokenizer/manifests/latest"));
        assert!(!is_token_path("/v2/user/token/manifests/latest"));
        assert!(!is_token_path("/v2/token/"));
    }

    #[test]
    fn fixes_single_segment_hub_scope() {
        assert_eq!(
            fix_hub_scope("scope=repository%3Abusybox%3Apull&service=registry.docker.io"),
            "scope=repository%3Alibrary%2Fbusybox%3Apull&service=registry.docker.io"
        );
        assert_eq!(
            fix_hub_scope("scope=repository%3abusybox%3apull&service=x"),
            "scope=repository%3Alibrary%2Fbusybox%3apull&service=x"
        );
        for unchanged in [
            "scope=repository%3Auser%2Frepo%3Apull&service=x",
            "scope=repository%3Abusybox%3Apull",
            "service=registry.docker.io&scope=repository%3Abusybox%3Apull",
            "scope=repository:busybox:pull&service=x",
        ] {
            assert_eq!(fix_hub_scope(unchanged), unchanged);
        }
    }

    #[test]
    fn adds_library_prefix_to_single_segment_repositories() {
        assert_eq!(
            with_library_prefix("/v2/nginx/manifests/latest"),
            "/v2/library/nginx/manifests/latest"
        );
        assert_eq!(
            with_library_prefix("/v2/librarything/manifests/latest"),
            "/v2/library/librarything/manifests/latest"
        );
        for unchanged in [
            "/v2/library/nginx/manifests/latest",
            "/v2/user/repo/manifests/latest",
            "/v2/nginx//latest",
            "/v2/nginx/manifests/",
            "/v2/",
            "/token",
        ] {
            assert_eq!(with_library_prefix(unchanged), unchanged);
        }
    }

    #[test]
    fn extracts_shortest_repository() {
        assert_eq!(
            repository("/v2/library/nginx/manifests/latest"),
            Some("library/nginx")
        );
        assert_eq!(repository("/v2/a/b/c/blobs/sha256:abc"), Some("a/b/c"));
        assert_eq!(repository("/v2/repo/tags/list"), Some("repo"));
        assert_eq!(repository("/v2/repo/manifests/x/blobs/y"), Some("repo"));
        assert_eq!(repository("/v2/blobs/blobs/x"), Some("blobs"));
        assert_eq!(repository("/v2/manifests/latest"), None);
        assert_eq!(repository("/v2//manifests/latest"), None);
        assert_eq!(repository("/v3/repo/manifests/latest"), None);
    }

    #[test]
    fn detects_content_paths() {
        assert!(is_content_path("/v2/repo/manifests/latest"));
        assert!(is_content_path("/v2/repo/tags/list"));
        assert!(!is_content_path("/v2/"));
        assert!(!is_content_path("/v2/_catalog"));
        assert!(!is_content_path("/v3/repo/blobs/x"));
    }

    #[test]
    fn builds_self_url_from_forwarded_proto() {
        let mut headers = HeaderMap::new();
        assert_eq!(self_url(&headers, "mirror.test"), "http://mirror.test");
        headers.insert("x-forwarded-proto", HeaderValue::from_static("HTTPS, http"));
        assert_eq!(self_url(&headers, "mirror.test"), "https://mirror.test");
        headers.insert("x-forwarded-proto", HeaderValue::from_static("http, https"));
        assert_eq!(self_url(&headers, "mirror.test"), "http://mirror.test");
        headers.insert("x-forwarded-proto", HeaderValue::from_static("wss"));
        assert_eq!(self_url(&headers, ""), "http://");
    }

    #[test]
    fn builds_base_headers() {
        let defaults = base_headers(&HeaderMap::new());
        assert_eq!(defaults[USER_AGENT], "Docker-Client");
        assert_eq!(defaults[ACCEPT], "*/*");
        assert_eq!(defaults[CACHE_CONTROL], "max-age=0");
        assert!(!defaults.contains_key(ACCEPT_LANGUAGE));
        assert!(!defaults.contains_key(ACCEPT_ENCODING));

        let mut client = HeaderMap::new();
        client.insert(USER_AGENT, HeaderValue::from_static("docker/27"));
        client.insert(ACCEPT_ENCODING, HeaderValue::from_static("gzip"));
        client.insert(AUTHORIZATION, HeaderValue::from_static("Bearer secret"));
        client.insert("x-custom", HeaderValue::from_static("1"));
        let headers = base_headers(&client);
        assert_eq!(headers[USER_AGENT], "docker/27");
        assert_eq!(headers[ACCEPT_ENCODING], "gzip");
        assert_eq!(headers.len(), 4);

        let mut client = HeaderMap::new();
        client.append(
            ACCEPT,
            HeaderValue::from_static("application/vnd.oci.image.index.v1+json"),
        );
        client.append(
            ACCEPT,
            HeaderValue::from_static("application/vnd.oci.image.manifest.v1+json"),
        );
        let headers = base_headers(&client);
        let accept: Vec<_> = headers.get_all(ACCEPT).iter().collect();
        assert_eq!(
            accept,
            [
                "application/vnd.oci.image.index.v1+json",
                "application/vnd.oci.image.manifest.v1+json"
            ]
        );
    }

    #[test]
    fn detects_non_canonical_targets() {
        assert!(is_canonical_target(
            "/v2/library/nginx/manifests/sha256:abc",
            None
        ));
        assert!(is_canonical_target(
            "/token",
            Some("scope=repository%3Abusybox%3Apull&service=x")
        ));
        assert!(is_canonical_target("/v2/", Some("")));
        for (path, query) in [
            ("/v2/library/nginx/manifests/../../../_catalog", None),
            ("/v2/library/nginx/manifests/%2e%2E/x", None),
            ("/v2/foo\\bar/manifests/latest", None),
            ("/v2/foo{bar}/manifests/latest", None),
            ("/v2/_catalog", Some("n='1'")),
        ] {
            assert!(!is_canonical_target(path, query), "{path} {query:?}");
        }
    }

    #[test]
    fn collects_error_sources() {
        #[derive(Debug)]
        struct RequestFailed(std::io::Error);
        impl std::fmt::Display for RequestFailed {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("request failed")
            }
        }
        impl std::error::Error for RequestFailed {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let err = RequestFailed(std::io::Error::other("connection refused"));
        assert!(matches!(
            UpstreamError::from(err),
            UpstreamError::Failed(message) if message == "request failed: connection refused"
        ));
    }
}
