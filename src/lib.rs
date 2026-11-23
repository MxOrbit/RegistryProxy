use std::{sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{Request, State},
    http::{StatusCode, header},
    middleware::{Next, from_fn_with_state},
    response::{IntoResponse, Response},
    routing::get,
};

mod auth;
pub mod config;
mod proxy;

use config::AuthMode;
pub use config::Config;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// Upstream client that never follows redirects.
    pub client: reqwest::Client,
    /// Upstream client that follows up to 10 redirects, used for blob downloads.
    pub follow_client: reqwest::Client,
    basic_auth: Option<Arc<[u8]>>,
}

impl AppState {
    pub fn new(config: Config) -> reqwest::Result<Self> {
        Self::with_client_options(config, |builder| builder)
    }

    /// Like [`AppState::new`], with a hook to adjust both upstream clients
    /// (integration tests use it to resolve registry hostnames to mock servers).
    pub fn with_client_options(
        config: Config,
        customize: impl Fn(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
    ) -> reqwest::Result<Self> {
        let client = customize(http_client_builder()?.redirect(reqwest::redirect::Policy::none()))
            .build()?;
        let follow_client =
            customize(http_client_builder()?.redirect(reqwest::redirect::Policy::limited(10)))
                .build()?;
        let basic_auth = match &config.auth {
            AuthMode::Basic { user, pass } => Some(Arc::from(auth::expected_header(user, pass))),
            AuthMode::Passthrough => None,
        };
        Ok(AppState {
            config: Arc::new(config),
            client,
            follow_client,
            basic_auth,
        })
    }
}

/// Shared reqwest settings: ring + bundled Mozilla roots, no automatic decompression,
/// no environment proxies, HTTP/1.1, connect and read-idle timeouts but no total timeout.
pub fn http_client_builder() -> reqwest::Result<reqwest::ClientBuilder> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let roots = webpki_root_certs::TLS_SERVER_ROOT_CERTS
        .iter()
        .map(|cert| reqwest::Certificate::from_der(cert.as_ref()))
        .collect::<reqwest::Result<Vec<_>>>()?;
    Ok(reqwest::Client::builder()
        .tls_certs_only(roots)
        .no_proxy()
        .connect_timeout(Duration::from_secs(10))
        .read_timeout(Duration::from_secs(60)))
}

pub fn router(state: AppState) -> Router {
    let proxied = Router::new()
        .fallback(proxy::handle)
        .layer(from_fn_with_state(state.clone(), block_user_agent))
        .layer(from_fn_with_state(state.clone(), auth::basic_auth))
        .with_state(state);

    Router::new()
        .route("/healthz", get(healthz))
        .fallback_service(proxied)
}

async fn healthz() -> &'static str {
    "ok"
}

async fn block_user_agent(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let user_agent = req
        .headers()
        .get(header::USER_AGENT)
        .map(|value| String::from_utf8_lossy(value.as_bytes()).to_lowercase())
        .unwrap_or_default();
    if state
        .config
        .blocked_ua
        .iter()
        .any(|blocked| user_agent.contains(blocked.as_str()))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    next.run(req).await
}
