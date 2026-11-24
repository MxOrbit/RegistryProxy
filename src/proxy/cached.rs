use std::sync::Arc;

use axum::{
    body::Body,
    http::{
        HeaderValue, Method, StatusCode,
        header::{
            ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_EXPOSE_HEADERS, CONTENT_ENCODING,
            CONTENT_LENGTH, CONTENT_TYPE, ETAG,
        },
    },
    response::Response,
};
use tracing::debug;

use super::{Proxy, UpstreamError};
use crate::cache::{self, Cache, Join, Lookup, Source};

impl Proxy<'_> {
    /// Serves a blob from the cache, joining or starting a single upstream download on a miss.
    pub(super) async fn cached_blob(
        &self,
        cache: &Arc<Cache>,
        hex: &str,
        path: &str,
        query: Option<&str>,
    ) -> Result<Response, UpstreamError> {
        match cache.lookup(hex).await {
            Lookup::Hit(source) => {
                debug!(digest = hex, "cache hit");
                return Ok(self.blob_response(hex, source));
            }
            Lookup::Unusable => return self.passthrough_content(path, query).await,
            Lookup::Miss => debug!(digest = hex, "cache miss"),
        }
        if self.method != Method::GET || cache.is_paused() {
            return self.passthrough_content(path, query).await;
        }
        let filler = match cache.join(hex) {
            Join::Owner(filler) => filler,
            Join::Follower(progress) => {
                return match cache::follow(progress).await {
                    Some(source) => Ok(self.blob_response(hex, source)),
                    None => self.passthrough_content(path, query).await,
                };
            }
        };
        // A fill may have published the blob between the lookup and the join.
        match cache.lookup(hex).await {
            Lookup::Hit(source) => {
                filler.complete(&source);
                return Ok(self.blob_response(hex, source));
            }
            Lookup::Unusable => {
                drop(filler);
                return self.passthrough_content(path, query).await;
            }
            Lookup::Miss => {}
        }
        let (status, headers, upstream) = self.fetch_content(path, query).await?;
        let length = headers
            .get(CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok());
        let identity = headers
            .get(CONTENT_ENCODING)
            .is_none_or(|value| value == "identity");
        if status == StatusCode::OK
            && identity
            && let Some(length) = length
        {
            return Ok(match filler.start(length, upstream) {
                Ok(source) => self.blob_response(hex, source),
                Err(upstream) => self.client_response(status, headers, upstream),
            });
        }
        drop(filler);
        Ok(self.client_response(status, headers, upstream))
    }

    async fn passthrough_content(
        &self,
        path: &str,
        query: Option<&str>,
    ) -> Result<Response, UpstreamError> {
        let (status, headers, upstream) = self.fetch_content(path, query).await?;
        Ok(self.client_response(status, headers, upstream))
    }

    fn blob_response(&self, hex: &str, source: Source) -> Response {
        let length = source.len();
        let body = if self.method == Method::HEAD {
            Body::empty()
        } else {
            source.into_body()
        };
        let digest = format!("sha256:{hex}");
        let mut response = Response::new(body);
        let headers = response.headers_mut();
        headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
        headers.insert(
            CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        headers.insert(
            ETAG,
            HeaderValue::from_str(&format!("\"{digest}\"")).unwrap(),
        );
        headers.insert(
            "docker-content-digest",
            HeaderValue::from_str(&digest).unwrap(),
        );
        headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
        headers.insert(ACCESS_CONTROL_EXPOSE_HEADERS, HeaderValue::from_static("*"));
        response
    }
}
