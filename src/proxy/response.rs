use axum::{
    body::Body,
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{
            ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, CACHE_CONTROL, Entry,
            LOCATION, WWW_AUTHENTICATE,
        },
    },
    response::Response,
};
use reqwest::Url;

use super::{Proxy, UpstreamError};

const DROPPED_HEADERS: [&str; 10] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "content-security-policy",
    "content-security-policy-report-only",
    "clear-site-data",
];

impl Proxy<'_> {
    /// Turns an upstream response into the client response, optionally following its redirect.
    pub(super) async fn respond(
        &self,
        upstream: reqwest::Response,
        rewrite_from: &str,
        follow_redirects: bool,
    ) -> Result<Response, UpstreamError> {
        let (status, headers, upstream) = self
            .prepare(upstream, rewrite_from, follow_redirects)
            .await?;
        Ok(self.client_response(status, headers, upstream))
    }

    /// Status and filtered headers for the client, plus the response whose body they belong to.
    pub(super) async fn prepare(
        &self,
        upstream: reqwest::Response,
        rewrite_from: &str,
        follow_redirects: bool,
    ) -> Result<(StatusCode, HeaderMap, reqwest::Response), UpstreamError> {
        if follow_redirects && let Some(location) = redirect_target(&upstream)? {
            return self.follow(upstream, location).await;
        }
        let mut headers = filtered_headers(upstream.headers());
        rewrite_www_authenticate(&mut headers, rewrite_from, &self.self_url);
        Ok((upstream.status(), headers, upstream))
    }

    async fn follow(
        &self,
        first: reqwest::Response,
        location: Url,
    ) -> Result<(StatusCode, HeaderMap, reqwest::Response), UpstreamError> {
        let last = self
            .send(
                &self.state.follow_client,
                self.method.clone(),
                location,
                self.base_headers.clone(),
            )
            .await?;
        let mut headers = filtered_headers(last.headers());
        copy_missing_docker_headers(first.headers(), &mut headers);
        headers.insert(CACHE_CONTROL, HeaderValue::from_static("max-age=1500"));
        Ok((last.status(), headers, last))
    }

    pub(super) fn client_response(
        &self,
        status: StatusCode,
        mut headers: HeaderMap,
        upstream: reqwest::Response,
    ) -> Response {
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
        headers.insert(ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("*"));
        let body = if self.method == Method::HEAD {
            Body::empty()
        } else {
            Body::from_stream(upstream.bytes_stream())
        };
        let mut response = Response::new(body);
        *response.status_mut() = status;
        *response.headers_mut() = headers;
        response
    }
}

/// The resolved `Location` when the response is a redirect (3xx, or an absolute `http` URL).
fn redirect_target(response: &reqwest::Response) -> Result<Option<Url>, UpstreamError> {
    let Some(location) = response
        .headers()
        .get(LOCATION)
        .and_then(|value| std::str::from_utf8(value.as_bytes()).ok())
    else {
        return Ok(None);
    };
    if !response.status().is_redirection() && !location.starts_with("http") {
        return Ok(None);
    }
    Ok(Some(response.url().join(location)?))
}

fn filtered_headers(source: &HeaderMap) -> HeaderMap {
    let mut headers = HeaderMap::with_capacity(source.len());
    for (name, value) in source {
        if !DROPPED_HEADERS.contains(&name.as_str()) {
            headers.append(name.clone(), value.clone());
        }
    }
    headers
}

fn copy_missing_docker_headers(first: &HeaderMap, headers: &mut HeaderMap) {
    for name in first.keys() {
        if name.as_str().starts_with("docker-") && !headers.contains_key(name) {
            for value in first.get_all(name) {
                headers.append(name.clone(), value.clone());
            }
        }
    }
}

fn rewrite_www_authenticate(headers: &mut HeaderMap, from: &str, to: &str) {
    if let Entry::Occupied(mut entry) = headers.entry(WWW_AUTHENTICATE) {
        for value in entry.iter_mut() {
            if let Some(rewritten) = replace_in_value(value, from, to) {
                *value = rewritten;
            }
        }
    }
}

fn replace_in_value(value: &HeaderValue, from: &str, to: &str) -> Option<HeaderValue> {
    let text = std::str::from_utf8(value.as_bytes()).ok()?;
    if from.is_empty() || !text.contains(from) {
        return None;
    }
    HeaderValue::from_str(&text.replace(from, to)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_hop_by_hop_and_security_headers() {
        let mut source = HeaderMap::new();
        for name in DROPPED_HEADERS {
            source.insert(name, HeaderValue::from_static("x"));
        }
        source.insert("content-encoding", HeaderValue::from_static("gzip"));
        source.insert("content-length", HeaderValue::from_static("42"));
        source.append(
            "docker-content-digest",
            HeaderValue::from_static("sha256:a"),
        );
        source.append("link", HeaderValue::from_static("<a>"));
        source.append("link", HeaderValue::from_static("<b>"));

        let headers = filtered_headers(&source);
        assert_eq!(headers.len(), 5);
        assert_eq!(headers["content-encoding"], "gzip");
        assert_eq!(headers["content-length"], "42");
        assert_eq!(headers.get_all("link").iter().count(), 2);
    }

    #[test]
    fn copies_only_missing_docker_headers() {
        let mut first = HeaderMap::new();
        first.insert(
            "docker-content-digest",
            HeaderValue::from_static("sha256:a"),
        );
        first.insert(
            "docker-distribution-api-version",
            HeaderValue::from_static("registry/2.0"),
        );
        first.insert(
            "location",
            HeaderValue::from_static("https://cdn.test/blob"),
        );
        first.insert("content-length", HeaderValue::from_static("0"));
        let mut headers = HeaderMap::new();
        headers.insert(
            "docker-distribution-api-version",
            HeaderValue::from_static("final"),
        );
        headers.insert("content-length", HeaderValue::from_static("42"));

        copy_missing_docker_headers(&first, &mut headers);
        assert_eq!(headers["docker-content-digest"], "sha256:a");
        assert_eq!(headers["docker-distribution-api-version"], "final");
        assert_eq!(headers["content-length"], "42");
        assert!(!headers.contains_key("location"));
    }

    #[test]
    fn rewrites_every_www_authenticate_occurrence() {
        let mut headers = HeaderMap::new();
        headers.append(
            WWW_AUTHENTICATE,
            HeaderValue::from_static(
                r#"Bearer realm="https://auth.docker.io/token",service="https://auth.docker.io""#,
            ),
        );
        headers.append(
            WWW_AUTHENTICATE,
            HeaderValue::from_static(r#"Basic realm="other""#),
        );
        headers.append(
            WWW_AUTHENTICATE,
            HeaderValue::from_bytes(b"Bearer realm=\"\xff\"").unwrap(),
        );

        rewrite_www_authenticate(
            &mut headers,
            "https://auth.docker.io",
            "https://mirror.test",
        );
        let values: Vec<_> = headers.get_all(WWW_AUTHENTICATE).iter().collect();
        assert_eq!(
            values[0],
            r#"Bearer realm="https://mirror.test/token",service="https://mirror.test""#
        );
        assert_eq!(values[1], r#"Basic realm="other""#);
        assert_eq!(values[2].as_bytes(), b"Bearer realm=\"\xff\"");
    }
}
