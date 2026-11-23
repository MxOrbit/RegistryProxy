use std::collections::HashMap;

use axum::http::{
    HeaderMap, HeaderValue, Method,
    header::{ACCEPT_ENCODING, WWW_AUTHENTICATE},
};
use reqwest::Url;

use super::{Proxy, UpstreamError};

impl Proxy<'_> {
    /// Base headers for requests whose bodies the proxy reads itself. The client's
    /// `Accept-Encoding` is dropped because the proxy does not decompress.
    fn internal_headers(&self) -> HeaderMap {
        let mut headers = self.base_headers.clone();
        headers.remove(ACCEPT_ENCODING);
        headers
    }

    /// Anonymous pull token from the Docker Hub auth service. Network errors and
    /// non-JSON bodies are fatal; a body without a token means "no token".
    pub(super) async fn hub_token(&self, repo: &str) -> Result<Option<HeaderValue>, UpstreamError> {
        let url = Url::parse(&format!(
            "{}/token?service=registry.docker.io&scope=repository:{repo}:pull",
            self.state.config.auth_url
        ))?;
        let response = self
            .send(
                &self.state.client,
                Method::GET,
                url,
                self.internal_headers(),
            )
            .await?;
        let body = response.bytes().await?;
        Ok(bearer_from_json(&body)?)
    }

    /// Probes `/v2/` for a challenge and exchanges it for a pull token on `repo`.
    pub(super) async fn registry_token(
        &self,
        repo: &str,
    ) -> Result<Option<HeaderValue>, UpstreamError> {
        let url = Url::parse(&format!("{}/v2/", self.registry_origin()))?;
        let probe = self
            .send(
                &self.state.client,
                Method::GET,
                url,
                self.internal_headers(),
            )
            .await?;
        let Some(challenge) = probe.headers().get(WWW_AUTHENTICATE).cloned() else {
            return Ok(None);
        };
        drop(probe);
        Ok(self.challenge_token(&challenge, Some(repo)).await)
    }

    /// Follows a `WWW-Authenticate: Bearer` challenge; every failure yields no token.
    pub(super) async fn challenge_token(
        &self,
        header: &HeaderValue,
        repo: Option<&str>,
    ) -> Option<HeaderValue> {
        let challenge = std::str::from_utf8(header.as_bytes())
            .ok()
            .and_then(Challenge::parse)?;
        let url = challenge.token_url(repo)?;
        let response = self
            .send(
                &self.state.client,
                Method::GET,
                url,
                self.internal_headers(),
            )
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.bytes().await.ok()?;
        bearer_from_json(&body).ok().flatten()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Challenge {
    realm: String,
    service: Option<String>,
    scope: Option<String>,
}

impl Challenge {
    /// Parses the `key="value"` pairs after the first `Bearer<whitespace>`; later keys win.
    fn parse(header: &str) -> Option<Self> {
        let lower = header.to_ascii_lowercase();
        let start = lower
            .match_indices("bearer")
            .map(|(index, _)| index + "bearer".len())
            .find(|&end| header[end..].starts_with(|next: char| next.is_whitespace()))?;
        let mut params = quoted_params(&header[start..]);
        Some(Challenge {
            realm: params.remove("realm")?.to_string(),
            service: params.remove("service").map(str::to_string),
            scope: params.remove("scope").map(str::to_string),
        })
    }

    fn token_url(&self, repo: Option<&str>) -> Option<Url> {
        let mut url = Url::parse(&self.realm).ok()?;
        if let Some(service) = &self.service {
            set_query_param(&mut url, "service", service);
        }
        match (repo, &self.scope) {
            (Some(repo), _) => {
                set_query_param(&mut url, "scope", &format!("repository:{repo}:pull"))
            }
            (None, Some(scope)) => set_query_param(&mut url, "scope", scope),
            (None, None) => {}
        }
        Some(url)
    }
}

/// Every non-overlapping match of `(\w+)="([^"]+)"`, scanning left to right.
fn quoted_params(text: &str) -> HashMap<&str, &str> {
    let bytes = text.as_bytes();
    let is_word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
    let mut params = HashMap::new();
    let mut index = 0;
    while index < bytes.len() {
        if !is_word(bytes[index]) {
            index += 1;
            continue;
        }
        let key_start = index;
        while index < bytes.len() && is_word(bytes[index]) {
            index += 1;
        }
        let key = &text[key_start..index];
        let Some(after_key) = text[index..].strip_prefix("=\"") else {
            continue;
        };
        let value_start = index + 2;
        let Some(length) = after_key.find('"') else {
            break;
        };
        if length == 0 {
            index = value_start;
            continue;
        }
        params.insert(key, &text[value_start..value_start + length]);
        index = value_start + length + 1;
    }
    params
}

/// Same semantics as `URLSearchParams.set`: replaces the first occurrence in place,
/// drops the rest, or appends; the whole query is re-serialized with form encoding.
fn set_query_param(url: &mut Url, name: &str, value: &str) {
    let mut replaced = false;
    let mut pairs = Vec::new();
    for (key, current) in url.query_pairs().into_owned() {
        if key != name {
            pairs.push((key, current));
        } else if !replaced {
            pairs.push((key, value.to_string()));
            replaced = true;
        }
    }
    if !replaced {
        pairs.push((name.to_string(), value.to_string()));
    }
    url.query_pairs_mut().clear().extend_pairs(pairs);
}

/// Extracts `token`, else `access_token`, as a ready `Bearer` header value.
fn bearer_from_json(body: &[u8]) -> Result<Option<HeaderValue>, serde_json::Error> {
    let json: serde_json::Value = serde_json::from_slice(body)?;
    Ok(["token", "access_token"]
        .into_iter()
        .find_map(|key| json.get(key)?.as_str().filter(|token| !token.is_empty()))
        .and_then(|token| HeaderValue::try_from(format!("Bearer {token}")).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn challenge(header: &str) -> Option<Challenge> {
        Challenge::parse(header)
    }

    #[test]
    fn parses_bearer_challenges() {
        assert_eq!(
            challenge(
                r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:user/image:pull""#
            ),
            Some(Challenge {
                realm: "https://ghcr.io/token".into(),
                service: Some("ghcr.io".into()),
                scope: Some("repository:user/image:pull".into()),
            })
        );
        assert_eq!(
            challenge("Basic realm=\"x\", bearer\trealm=\"https://auth.test/t\", service=\"\""),
            Some(Challenge {
                realm: "https://auth.test/t".into(),
                service: None,
                scope: None,
            })
        );
        assert_eq!(
            challenge(r#"Bearer realm="first",realm="second""#).map(|parsed| parsed.realm),
            Some("second".into())
        );
    }

    #[test]
    fn rejects_challenges_without_bearer_or_realm() {
        assert_eq!(challenge(r#"Basic realm="Registry""#), None);
        assert_eq!(challenge(r#"Bearer,realm="https://auth.test""#), None);
        assert_eq!(challenge(r#"Bearer service="registry""#), None);
        assert_eq!(challenge(r#"Bearer realm=unquoted"#), None);
        assert_eq!(challenge(r#"Bearer realm="unterminated"#), None);
        assert_eq!(challenge(""), None);
    }

    #[test]
    fn scans_params_like_the_original_regex() {
        let params = quoted_params(r#" a b="1", c=x, d="", e="2",f="3"#);
        assert_eq!(params.get("b"), Some(&"1"));
        assert_eq!(params.get("e"), Some(&"2"));
        assert_eq!(params.len(), 2);
    }

    #[test]
    fn builds_token_urls_with_form_encoding() {
        let parsed = Challenge {
            realm: "https://auth.test/token?scope=old&x=1&scope=dup".into(),
            service: Some("registry.test".into()),
            scope: Some("repository:other:pull".into()),
        };
        assert_eq!(
            parsed.token_url(Some("user/image")).unwrap().as_str(),
            "https://auth.test/token?scope=repository%3Auser%2Fimage%3Apull&x=1&service=registry.test"
        );
        assert_eq!(
            parsed.token_url(None).unwrap().as_str(),
            "https://auth.test/token?scope=repository%3Aother%3Apull&x=1&service=registry.test"
        );

        let bare = Challenge {
            realm: "https://auth.test/token".into(),
            service: None,
            scope: None,
        };
        assert_eq!(
            bare.token_url(None).unwrap().as_str(),
            "https://auth.test/token"
        );
        let invalid = Challenge {
            realm: "/relative".into(),
            service: None,
            scope: None,
        };
        assert_eq!(invalid.token_url(None), None);
    }

    #[test]
    fn reads_tokens_from_json() {
        assert_eq!(
            bearer_from_json(br#"{"token":"abc","access_token":"def"}"#)
                .unwrap()
                .unwrap(),
            "Bearer abc"
        );
        assert_eq!(
            bearer_from_json(br#"{"token":"","access_token":"def"}"#)
                .unwrap()
                .unwrap(),
            "Bearer def"
        );
        assert_eq!(bearer_from_json(br#"{"other":1}"#).unwrap(), None);
        assert_eq!(bearer_from_json(br#"[]"#).unwrap(), None);
        assert_eq!(
            bearer_from_json(br#"{"token":"bad\nvalue"}"#).unwrap(),
            None
        );
        assert!(bearer_from_json(b"<html>").is_err());
    }
}
