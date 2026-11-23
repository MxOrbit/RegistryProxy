#![allow(dead_code)]

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    Router,
    extract::Request,
    http::{HeaderMap, Method, Uri},
    middleware::{self, Next},
};
use registry_proxy::{AppState, Config, router};

/// A request observed by a mock upstream.
#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: Method,
    pub uri: Uri,
    pub headers: HeaderMap,
}

impl Recorded {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// Path plus query exactly as received, e.g. `/v2/library/nginx/manifests/latest?ns=docker.io`.
    pub fn path_and_query(&self) -> &str {
        self.uri.path_and_query().map_or("", |pq| pq.as_str())
    }
}

#[derive(Clone, Default)]
pub struct Recorder(Arc<Mutex<Vec<Recorded>>>);

impl Recorder {
    pub fn requests(&self) -> Vec<Recorded> {
        self.0.lock().unwrap().clone()
    }

    pub fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    pub fn last(&self) -> Recorded {
        self.0
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("mock received no request")
    }
}

/// Serves `app` on 127.0.0.1 with a random port and records every request it receives.
pub async fn spawn_mock(app: Router) -> (SocketAddr, Recorder) {
    let recorder = Recorder::default();
    let sink = recorder.clone();
    let app = app.layer(middleware::from_fn(move |req: Request, next: Next| {
        let sink = sink.clone();
        async move {
            sink.0.lock().unwrap().push(Recorded {
                method: req.method().clone(),
                uri: req.uri().clone(),
                headers: req.headers().clone(),
            });
            next.run(req).await
        }
    }));
    (serve(app).await, recorder)
}

async fn serve(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Builds a config from the given env-style variables and switches upstream traffic to plain HTTP.
pub fn test_config(vars: &[(&str, &str)]) -> Config {
    let mut config = Config::from_lookup(|name| {
        vars.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.to_string())
    })
    .expect("valid test config");
    config.upstream_scheme = "http".to_string();
    config
}

pub struct TestProxy {
    pub addr: SocketAddr,
    /// Base URL of the proxy, e.g. `http://127.0.0.1:54321`.
    pub url: String,
    /// Client for talking to the proxy. Does not follow redirects or decompress bodies.
    pub client: reqwest::Client,
}

impl TestProxy {
    pub fn get(&self, path_and_query: &str) -> reqwest::RequestBuilder {
        self.client.get(format!("{}{}", self.url, path_and_query))
    }

    pub fn request(
        &self,
        method: reqwest::Method,
        path_and_query: &str,
    ) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}{}", self.url, path_and_query))
    }
}

/// Starts the proxy with `config`. Each `(hostname, addr)` pair makes the proxy's upstream
/// clients connect to `addr` whenever they target `hostname`
/// (e.g. `("registry-1.docker.io", mock)`, `("auth.docker.io", mock)`, `("cdn.test", mock)`).
pub async fn spawn_proxy(config: Config, resolve: &[(&str, SocketAddr)]) -> TestProxy {
    let overrides: Vec<(String, SocketAddr)> = resolve
        .iter()
        .map(|(host, addr)| (host.to_string(), *addr))
        .collect();
    let state = AppState::with_client_options(config, move |mut builder| {
        for (host, addr) in &overrides {
            builder = builder.resolve(host, *addr);
        }
        builder
    })
    .expect("build proxy state");
    let addr = serve(router(state)).await;
    let client = registry_proxy::http_client_builder()
        .unwrap()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    TestProxy {
        addr,
        url: format!("http://{addr}"),
        client,
    }
}
