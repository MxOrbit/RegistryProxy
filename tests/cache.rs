mod common;

use std::{
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use axum::{
    Router,
    body::{Body, Bytes},
    extract::Request,
    middleware::{self, Next},
    response::Response,
};
use common::{Recorder, TestProxy, spawn_mock, spawn_proxy, test_config};
use futures_util::StreamExt;
use registry_proxy::AppState;
use tokio::sync::watch;

const HUB_HOST: &str = "registry-1.docker.io";
const AUTH_URL: &str = "http://auth.docker.io";
const OCTET_STREAM: &str = "application/octet-stream";
const HOUR: Duration = Duration::from_secs(3600);
const MINUTE: Duration = Duration::from_secs(60);

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

fn token_router() -> Router {
    mock_router(|_| {
        reply(
            200,
            &[("content-type", "application/json")],
            r#"{"token":"t"}"#,
        )
    })
}

/// Serves `body` with status 200 for the Docker Hub upstream path of `hex`, 404 otherwise.
fn blob_router(hex: &str, body: Vec<u8>) -> Router {
    let path = upstream_blob_path(hex);
    mock_router(move |req| {
        if req.uri().path() != path {
            return reply(404, &[], "");
        }
        reply(
            200,
            &[
                ("content-type", OCTET_STREAM),
                ("content-length", &body.len().to_string()),
            ],
            body.clone(),
        )
    })
}

/// Serves `body` with status 200, sleeping `delay` before each `chunk`-sized piece.
fn slow_blob_router(body: Vec<u8>, chunk: usize, delay: Duration) -> Router {
    let body = Bytes::from(body);
    Router::new().fallback(move || {
        let body = body.clone();
        async move {
            let length = body.len();
            let pieces: Vec<Bytes> = (0..length)
                .step_by(chunk)
                .map(|start| body.slice(start..(start + chunk).min(length)))
                .collect();
            let stream = futures_util::stream::iter(pieces).then(move |piece| async move {
                tokio::time::sleep(delay).await;
                Ok::<_, std::io::Error>(piece)
            });
            reply(
                200,
                &[
                    ("content-type", OCTET_STREAM),
                    ("content-length", &length.to_string()),
                ],
                Body::from_stream(stream),
            )
        }
    })
}

/// Serves `body` with status 200: the first `head` bytes at once, the rest in small delayed
/// pieces only after `gate` opens.
fn gated_blob_router(body: Vec<u8>, head: usize, gate: watch::Receiver<bool>) -> Router {
    let body = Bytes::from(body);
    Router::new().fallback(move || {
        let body = body.clone();
        let gate = gate.clone();
        async move {
            let length = body.len();
            let stream = futures_util::stream::unfold((0, gate), move |(offset, mut gate)| {
                let body = body.clone();
                async move {
                    if offset == body.len() {
                        return None;
                    }
                    if offset > 0 {
                        let _ = gate.wait_for(|open| *open).await;
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    let end = if offset == 0 {
                        head
                    } else {
                        (offset + 64 * 1024).min(body.len())
                    };
                    Some((
                        Ok::<_, std::io::Error>(body.slice(offset..end)),
                        (end, gate),
                    ))
                }
            });
            reply(
                200,
                &[
                    ("content-type", OCTET_STREAM),
                    ("content-length", &length.to_string()),
                ],
                Body::from_stream(stream),
            )
        }
    })
}

/// Holds every response of `router` until `gate` opens.
fn gated(router: Router, gate: watch::Receiver<bool>) -> Router {
    router.layer(middleware::from_fn(move |req: Request, next: Next| {
        let mut gate = gate.clone();
        async move {
            let _ = gate.wait_for(|open| *open).await;
            next.run(req).await
        }
    }))
}

struct CacheDir(PathBuf);

impl CacheDir {
    fn new(name: &str) -> CacheDir {
        let path = std::env::temp_dir().join(format!(
            "registry-proxy-cache-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        let _ = fs::remove_file(&path);
        fs::create_dir_all(&path).unwrap();
        CacheDir(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn env(&self) -> &str {
        self.0.to_str().unwrap()
    }

    fn blob(&self, hex: &str) -> PathBuf {
        self.0.join("sha256").join(hex)
    }

    /// Writes `content` to `sha256/<hex>` with an mtime of `age` ago.
    fn put_blob(&self, hex: &str, content: &[u8], age: Duration) -> PathBuf {
        let path = self.blob(hex);
        write_aged(&path, content, age);
        path
    }

    fn tmp_is_empty(&self) -> bool {
        fs::read_dir(self.0.join("tmp")).map_or(true, |mut entries| entries.next().is_none())
    }
}

impl Drop for CacheDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn write_aged(path: &Path, content: &[u8], age: Duration) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
    fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - age)
        .unwrap();
}

fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path).unwrap().modified().unwrap()
}

struct World {
    proxy: TestProxy,
    hub: Recorder,
    auth: Recorder,
}

impl World {
    async fn start(dir: &CacheDir, vars: &[(&str, &str)], hub: Router) -> World {
        let (hub_addr, hub) = spawn_mock(hub).await;
        let (auth_addr, auth) = spawn_mock(token_router()).await;
        let mut all_vars = vec![("CACHE_DIR", dir.env()), ("AUTH_URL", AUTH_URL)];
        all_vars.extend_from_slice(vars);
        let proxy = spawn_proxy(
            test_config(&all_vars),
            &[(HUB_HOST, hub_addr), ("auth.docker.io", auth_addr)],
        )
        .await;
        World { proxy, hub, auth }
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(64);
    for byte in ring::digest::digest(&ring::digest::SHA256, bytes).as_ref() {
        write!(hex, "{byte:02x}").unwrap();
    }
    hex
}

/// A valid-looking 64-character lowercase hex name, e.g. `fake_hex(0x0a)` is `0a0a…0a`.
fn fake_hex(byte: u8) -> String {
    format!("{byte:02x}").repeat(32)
}

fn blob_path(hex: &str) -> String {
    format!("/v2/nginx/blobs/sha256:{hex}")
}

fn upstream_blob_path(hex: &str) -> String {
    format!("/v2/library/nginx/blobs/sha256:{hex}")
}

fn patterned_bytes(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect()
}

fn header<'a>(response: &'a reqwest::Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

fn assert_blob_headers(response: &reqwest::Response, hex: &str, length: usize) {
    let digest = format!("sha256:{hex}");
    assert_eq!(
        header(response, "content-length"),
        Some(length.to_string().as_str())
    );
    assert_eq!(header(response, "content-type"), Some(OCTET_STREAM));
    assert_eq!(
        header(response, "docker-content-digest"),
        Some(digest.as_str())
    );
    assert_eq!(
        header(response, "etag"),
        Some(format!("\"{digest}\"").as_str())
    );
    assert_eq!(header(response, "access-control-allow-origin"), Some("*"));
}

async fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn get_full(request: reqwest::RequestBuilder) -> (u16, Option<String>, Bytes) {
    let response = request.send().await.unwrap();
    let status = response.status().as_u16();
    let length = header(&response, "content-length").map(str::to_string);
    (status, length, response.bytes().await.unwrap())
}

#[tokio::test]
async fn get_hit_is_served_from_disk_without_upstream_requests() {
    let dir = CacheDir::new("get-hit");
    let blob = patterned_bytes(5000);
    let hex = sha256_hex(&blob);
    dir.put_blob(&hex, &blob, Duration::ZERO);
    let world = World::start(&dir, &[], blob_router(&hex, b"upstream".to_vec())).await;

    for path in [
        blob_path(&hex),
        format!("/v2/myorg/tools/blobs/sha256:{hex}"),
    ] {
        let response = world.proxy.get(&path).send().await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
        assert_blob_headers(&response, &hex, blob.len());
        assert_eq!(response.bytes().await.unwrap().as_ref(), blob.as_slice());
    }

    assert_eq!(world.hub.count(), 0);
    assert_eq!(world.auth.count(), 0);
}

#[tokio::test]
async fn head_hit_returns_blob_headers_with_empty_body() {
    let dir = CacheDir::new("head-hit");
    let blob = patterned_bytes(3210);
    let hex = sha256_hex(&blob);
    dir.put_blob(&hex, &blob, Duration::ZERO);
    let world = World::start(&dir, &[], blob_router(&hex, blob.clone())).await;

    let response = world
        .proxy
        .request(reqwest::Method::HEAD, &blob_path(&hex))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_blob_headers(&response, &hex, blob.len());
    assert!(response.bytes().await.unwrap().is_empty());

    assert_eq!(world.hub.count(), 0);
    assert_eq!(world.auth.count(), 0);
}

#[tokio::test]
async fn get_miss_streams_upstream_body_and_publishes_verified_blob() {
    let dir = CacheDir::new("get-miss");
    let blob = patterned_bytes(70_000);
    let hex = sha256_hex(&blob);
    let world = World::start(&dir, &[], blob_router(&hex, blob.clone())).await;

    let response = world.proxy.get(&blob_path(&hex)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_blob_headers(&response, &hex, blob.len());
    assert_eq!(response.bytes().await.unwrap().as_ref(), blob.as_slice());

    assert_eq!(world.hub.count(), 1);
    assert_eq!(world.hub.last().uri.path(), upstream_blob_path(&hex));
    assert_eq!(world.auth.count(), 1);

    let cached = dir.blob(&hex);
    eventually("blob to be published", || cached.is_file()).await;
    assert_eq!(fs::read(&cached).unwrap(), blob);
    eventually("tmp to be empty", || dir.tmp_is_empty()).await;

    let response = world.proxy.get(&blob_path(&hex)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_blob_headers(&response, &hex, blob.len());
    assert_eq!(response.bytes().await.unwrap().as_ref(), blob.as_slice());
    assert_eq!(world.hub.count(), 1);
    assert_eq!(world.auth.count(), 1);
}

#[tokio::test]
async fn head_miss_is_passed_through_and_not_cached() {
    let dir = CacheDir::new("head-miss");
    let blob = patterned_bytes(4096);
    let hex = sha256_hex(&blob);
    let world = World::start(&dir, &[], blob_router(&hex, blob.clone())).await;

    for expected_count in 1..=2 {
        let response = world
            .proxy
            .request(reqwest::Method::HEAD, &blob_path(&hex))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            header(&response, "content-length"),
            Some(blob.len().to_string().as_str())
        );
        assert!(response.bytes().await.unwrap().is_empty());

        assert_eq!(world.hub.count(), expected_count);
        let upstream = world.hub.last();
        assert_eq!(upstream.method, reqwest::Method::HEAD);
        assert_eq!(upstream.uri.path(), upstream_blob_path(&hex));
        assert!(!dir.blob(&hex).exists());
    }
}

#[tokio::test]
async fn range_request_bypasses_cached_blob() {
    let dir = CacheDir::new("range-hit");
    let blob = patterned_bytes(2000);
    let hex = sha256_hex(&blob);
    let cached = dir.put_blob(&hex, &blob, 2 * HOUR);
    let before = mtime(&cached);
    let world = World::start(&dir, &[], blob_router(&hex, b"upstream-bytes".to_vec())).await;

    let response = world
        .proxy
        .get(&blob_path(&hex))
        .header("Range", "bytes=0-3")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "upstream-bytes");

    assert_eq!(world.hub.count(), 1);
    assert_eq!(world.hub.last().uri.path(), upstream_blob_path(&hex));
    assert_eq!(fs::read(&cached).unwrap(), blob);
    assert_eq!(mtime(&cached), before);
}

#[tokio::test]
async fn range_request_miss_is_not_cached() {
    let dir = CacheDir::new("range-miss");
    let blob = patterned_bytes(6000);
    let hex = sha256_hex(&blob);
    let world = World::start(&dir, &[], blob_router(&hex, blob.clone())).await;

    let response = world
        .proxy
        .get(&blob_path(&hex))
        .header("Range", "bytes=0-")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), blob.as_slice());
    assert_eq!(world.hub.count(), 1);
    eventually("tmp to be empty", || dir.tmp_is_empty()).await;
    assert!(!dir.blob(&hex).exists());

    let response = world.proxy.get(&blob_path(&hex)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), blob.as_slice());
    assert_eq!(world.hub.count(), 2);
}

#[tokio::test]
async fn non_canonical_blob_paths_do_not_use_the_cache() {
    let dir = CacheDir::new("non-canonical");
    let hex = fake_hex(0x5a);
    let cached = dir.put_blob(&hex, b"cached", Duration::ZERO);
    let upper = hex.to_uppercase();
    let world = World::start(&dir, &[], mock_router(|_| reply(200, &[], "upstream"))).await;

    for (path, upstream_path) in [
        (
            format!("/v2/nginx/blobs/sha256:{upper}"),
            format!("/v2/library/nginx/blobs/sha256:{upper}"),
        ),
        (
            format!("/v2/nginx/manifests/sha256:{hex}"),
            format!("/v2/library/nginx/manifests/sha256:{hex}"),
        ),
    ] {
        let before = world.hub.count();
        let response = world.proxy.get(&path).send().await.unwrap();
        assert_eq!(response.status(), 200, "{path}");
        assert_eq!(response.text().await.unwrap(), "upstream", "{path}");
        assert_eq!(world.hub.count(), before + 1, "{path}");
        assert_eq!(world.hub.last().uri.path(), upstream_path);
    }
    assert_eq!(fs::read(&cached).unwrap(), b"cached");
}

#[tokio::test]
async fn digest_mismatch_is_delivered_but_not_stored() {
    let dir = CacheDir::new("mismatch");
    let served = patterned_bytes(10_000);
    let claimed = sha256_hex(b"something else");
    let world = World::start(&dir, &[], blob_router(&claimed, served.clone())).await;

    let response = world.proxy.get(&blob_path(&claimed)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), served.as_slice());
    eventually("tmp to be empty", || dir.tmp_is_empty()).await;
    assert!(!dir.blob(&claimed).exists());

    let response = world.proxy.get(&blob_path(&claimed)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.bytes().await.unwrap().as_ref(), served.as_slice());
    assert_eq!(world.hub.count(), 2);
    eventually("tmp to be empty", || dir.tmp_is_empty()).await;
    assert!(!dir.blob(&claimed).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn upstream_error_status_reaches_leader_and_follower() {
    let dir = CacheDir::new("upstream-404");
    let hex = fake_hex(0x44);
    let (open, gate) = watch::channel(false);
    let not_found = mock_router(|_| {
        reply(
            404,
            &[("content-type", "application/json")],
            r#"{"errors":[{"code":"BLOB_UNKNOWN","message":"blob unknown to registry"}]}"#,
        )
    });
    let world = World::start(&dir, &[], gated(not_found, gate)).await;

    let leader = tokio::spawn(get_full(world.proxy.get(&blob_path(&hex))));
    eventually("leader to reach upstream", || world.hub.count() == 1).await;
    let follower = tokio::spawn(get_full(world.proxy.get(&blob_path(&hex))));
    // A follower that skipped the fill would reach the (still closed) upstream as well.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(world.hub.count(), 1);
    assert!(!follower.is_finished());
    open.send(true).unwrap();

    assert_eq!(leader.await.unwrap().0, 404);
    assert_eq!(follower.await.unwrap().0, 404);
    assert_eq!(world.hub.count(), 2);
    assert!(
        world
            .hub
            .requests()
            .iter()
            .all(|recorded| recorded.uri.path() == upstream_blob_path(&hex))
    );
    assert!(!dir.blob(&hex).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_gets_during_slow_download_share_one_upstream_request() {
    let dir = CacheDir::new("concurrent-slow");
    let blob = patterned_bytes(256 * 1024);
    let hex = sha256_hex(&blob);
    let world = World::start(
        &dir,
        &[],
        slow_blob_router(blob.clone(), 16 * 1024, Duration::from_millis(50)),
    )
    .await;

    let first = world.proxy.get(&blob_path(&hex)).send().await.unwrap();
    assert_eq!(first.status(), 200);
    assert_blob_headers(&first, &hex, blob.len());
    let followers: Vec<_> = (0..3)
        .map(|_| tokio::spawn(get_full(world.proxy.get(&blob_path(&hex)))))
        .collect();

    assert_eq!(first.bytes().await.unwrap().as_ref(), blob.as_slice());
    for follower in followers {
        let (status, length, body) = follower.await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(length, Some(blob.len().to_string()));
        assert_eq!(body.as_ref(), blob.as_slice());
    }
    assert_eq!(world.hub.count(), 1);

    let cached = dir.blob(&hex);
    eventually("blob to be published", || cached.is_file()).await;
    assert_eq!(fs::read(&cached).unwrap(), blob);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn simultaneous_misses_share_one_upstream_request() {
    let dir = CacheDir::new("concurrent-start");
    let blob = patterned_bytes(128 * 1024);
    let hex = sha256_hex(&blob);
    let world = World::start(
        &dir,
        &[],
        slow_blob_router(blob.clone(), 16 * 1024, Duration::from_millis(40)),
    )
    .await;

    let clients: Vec<_> = (0..4)
        .map(|_| tokio::spawn(get_full(world.proxy.get(&blob_path(&hex)))))
        .collect();
    for client in clients {
        let (status, length, body) = client.await.unwrap();
        assert_eq!(status, 200);
        assert_eq!(length, Some(blob.len().to_string()));
        assert_eq!(body.as_ref(), blob.as_slice());
    }
    assert_eq!(world.hub.count(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn first_client_disconnect_does_not_stop_shared_download() {
    let dir = CacheDir::new("disconnect");
    let blob = patterned_bytes(2 * 1024 * 1024);
    let hex = sha256_hex(&blob);
    let (open, gate) = watch::channel(false);
    let world = World::start(&dir, &[], gated_blob_router(blob.clone(), 64 * 1024, gate)).await;

    let mut first = world.proxy.get(&blob_path(&hex)).send().await.unwrap();
    assert_eq!(first.status(), 200);
    let piece = first.chunk().await.unwrap().expect("first body chunk");
    assert!(!piece.is_empty());
    // The upstream holds the rest of the body, so these headers can only come from the fill.
    let second = world.proxy.get(&blob_path(&hex)).send().await.unwrap();
    assert_eq!(second.status(), 200);
    assert_eq!(world.hub.count(), 1);
    drop(first);
    open.send(true).unwrap();

    assert_blob_headers(&second, &hex, blob.len());
    assert_eq!(second.bytes().await.unwrap().as_ref(), blob.as_slice());
    assert_eq!(world.hub.count(), 1);

    let cached = dir.blob(&hex);
    eventually("blob to be published", || cached.is_file()).await;
    assert_eq!(fs::read(&cached).unwrap(), blob);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn download_is_abandoned_when_every_client_disconnects() {
    let dir = CacheDir::new("abandon");
    let blob = patterned_bytes(256 * 1024);
    let hex = sha256_hex(&blob);
    let (open, gate) = watch::channel(false);
    let world = World::start(&dir, &[], gated_blob_router(blob.clone(), 16 * 1024, gate)).await;

    let mut client = world.proxy.get(&blob_path(&hex)).send().await.unwrap();
    assert_eq!(client.status(), 200);
    assert!(client.chunk().await.unwrap().is_some());
    assert!(!dir.tmp_is_empty());
    drop(client);
    // The upstream still holds the rest of the body, so only abandoning the fill empties tmp.
    eventually("download to be abandoned", || dir.tmp_is_empty()).await;
    assert!(!dir.blob(&hex).exists());

    open.send(true).unwrap();
    let (status, _, body) = get_full(world.proxy.get(&blob_path(&hex))).await;
    assert_eq!(status, 200);
    assert_eq!(body.as_ref(), blob.as_slice());
    assert_eq!(world.hub.count(), 2);
    eventually("blob to be published", || dir.blob(&hex).is_file()).await;
}

#[tokio::test]
async fn truncated_upstream_body_fails_the_client_and_is_not_cached() {
    let dir = CacheDir::new("truncated");
    let blob = Bytes::from(patterned_bytes(64 * 1024));
    let hex = sha256_hex(&blob);
    let served = AtomicUsize::new(0);
    let upstream = blob.clone();
    let router = mock_router(move |_| {
        let body = if served.fetch_add(1, Ordering::Relaxed) == 0 {
            upstream.slice(..upstream.len() / 2)
        } else {
            upstream.clone()
        };
        reply(
            200,
            &[
                ("content-type", OCTET_STREAM),
                ("content-length", &upstream.len().to_string()),
            ],
            // Pause before ending so hyper flushes the headers and the short body first.
            Body::from_stream(futures_util::stream::unfold(
                Some(body),
                |body| async move {
                    match body {
                        Some(body) => Some((Ok::<_, std::io::Error>(body), None)),
                        None => {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            None
                        }
                    }
                },
            )),
        )
    });
    let world = World::start(&dir, &[], router).await;

    let response = world.proxy.get(&blob_path(&hex)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert!(response.bytes().await.is_err());
    eventually("tmp to be empty", || dir.tmp_is_empty()).await;
    assert!(!dir.blob(&hex).exists());

    let (status, _, body) = get_full(world.proxy.get(&blob_path(&hex))).await;
    assert_eq!(status, 200);
    assert_eq!(body, blob);
    assert_eq!(world.hub.count(), 2);
    eventually("blob to be published", || dir.blob(&hex).is_file()).await;
}

#[tokio::test]
async fn non_routing_query_bypasses_the_cache() {
    let dir = CacheDir::new("query");
    let cached_blob = patterned_bytes(3000);
    let cached_hex = sha256_hex(&cached_blob);
    dir.put_blob(&cached_hex, &cached_blob, Duration::ZERO);
    let upstream = b"upstream blob".to_vec();
    let upstream_hex = sha256_hex(&upstream);
    let world = World::start(&dir, &[], blob_router(&upstream_hex, upstream.clone())).await;

    let response = world
        .proxy
        .get(&format!("{}?ns=docker.io", blob_path(&cached_hex)))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        cached_blob.as_slice()
    );
    assert_eq!(world.hub.count(), 0);

    let response = world
        .proxy
        .get(&format!("{}?sig=abc", blob_path(&cached_hex)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert_eq!(world.hub.count(), 1);

    let response = world
        .proxy
        .get(&format!("{}?sig=abc", blob_path(&upstream_hex)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.bytes().await.unwrap().as_ref(),
        upstream.as_slice()
    );
    assert_eq!(world.hub.count(), 2);
    assert!(world.hub.last().path_and_query().ends_with("sig=abc"));
    assert!(dir.tmp_is_empty());
    assert!(!dir.blob(&upstream_hex).exists());
}

#[tokio::test]
async fn directory_at_blob_path_is_passed_through() {
    let dir = CacheDir::new("blob-dir");
    let blob = patterned_bytes(2000);
    let hex = sha256_hex(&blob);
    fs::create_dir_all(dir.blob(&hex)).unwrap();
    let world = World::start(&dir, &[], blob_router(&hex, blob.clone())).await;

    let response = world
        .proxy
        .request(reqwest::Method::HEAD, &blob_path(&hex))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(world.hub.count(), 1);
    assert_eq!(world.hub.last().method, reqwest::Method::HEAD);

    let (status, _, body) = get_full(world.proxy.get(&blob_path(&hex))).await;
    assert_eq!(status, 200);
    assert_eq!(body.as_ref(), blob.as_slice());
    assert_eq!(world.hub.count(), 2);
    assert!(dir.blob(&hex).is_dir());
    assert!(dir.tmp_is_empty());
}

#[tokio::test]
async fn startup_clears_tmp_and_leaves_foreign_entries_alone() {
    let dir = CacheDir::new("startup");
    let leftovers = [
        dir.path().join("tmp").join("partial"),
        dir.path().join("tmp").join(fake_hex(0x09)),
    ];
    for leftover in &leftovers {
        write_aged(leftover, b"partial", Duration::ZERO);
    }
    let sha256 = dir.path().join("sha256");
    let foreign_files = [
        sha256.join("not-a-digest"),
        sha256.join("abc"),
        sha256.join("CD".repeat(32)),
    ];
    for foreign in &foreign_files {
        write_aged(foreign, b"foreign", 2 * HOUR);
    }
    let subdir_file = sha256.join(fake_hex(0x07)).join("inner");
    write_aged(&subdir_file, b"inner", 2 * HOUR);
    let expired = dir.put_blob(&fake_hex(0x01), b"expired", 2 * HOUR);
    let blob = patterned_bytes(1234);
    let hex = sha256_hex(&blob);
    dir.put_blob(&hex, &blob, Duration::ZERO);

    let world = World::start(&dir, &[("CACHE_TTL", "1h")], blob_router(&hex, Vec::new())).await;

    eventually("startup cleanup and GC", || {
        !expired.exists() && leftovers.iter().all(|leftover| !leftover.exists())
    })
    .await;
    for foreign in &foreign_files {
        assert_eq!(fs::read(foreign).unwrap(), b"foreign", "{foreign:?}");
    }
    assert!(sha256.join(fake_hex(0x07)).is_dir());
    assert_eq!(fs::read(&subdir_file).unwrap(), b"inner");

    let response = world.proxy.get(&blob_path(&hex)).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_blob_headers(&response, &hex, blob.len());
    assert_eq!(response.bytes().await.unwrap().as_ref(), blob.as_slice());
    assert_eq!(world.hub.count(), 0);
}

#[tokio::test]
async fn cache_dir_pointing_to_a_regular_file_is_rejected() {
    let dir = CacheDir::new("regular-file");
    let file = dir.path().join("file");
    fs::write(&file, b"not a directory").unwrap();

    let config = test_config(&[("CACHE_DIR", file.to_str().unwrap())]);
    let result: Result<AppState, String> = AppState::new(config);
    let Err(message) = result else {
        panic!("AppState::new accepted a regular file as CACHE_DIR");
    };
    assert!(!message.is_empty());
    assert_eq!(fs::read(&file).unwrap(), b"not a directory");
}

#[tokio::test]
async fn gc_threshold_evicts_least_recently_used_blobs() {
    let dir = CacheDir::new("gc-threshold");
    let content = vec![0_u8; 1000];
    let oldest = dir.put_blob(&fake_hex(0x01), &content, 3 * HOUR);
    let middle = dir.put_blob(&fake_hex(0x02), &content, 2 * HOUR);
    let newest = dir.put_blob(&fake_hex(0x03), &content, HOUR);

    let _world = World::start(
        &dir,
        &[("CACHE_GC_THRESHOLD", "1000:2500")],
        blob_router(&fake_hex(0x03), Vec::new()),
    )
    .await;

    eventually("two oldest blobs to be evicted", || {
        !oldest.exists() && !middle.exists()
    })
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(newest.is_file());
}

#[tokio::test]
async fn gc_ttl_deletes_only_expired_blobs() {
    let dir = CacheDir::new("gc-ttl");
    let expired = dir.put_blob(&fake_hex(0x01), b"old", 2 * HOUR);
    let recent = dir.put_blob(&fake_hex(0x02), b"recent", 10 * MINUTE);

    let _world = World::start(
        &dir,
        &[("CACHE_TTL", "1h")],
        blob_router(&fake_hex(0x02), Vec::new()),
    )
    .await;

    eventually("expired blob to be deleted", || !expired.exists()).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(fs::read(&recent).unwrap(), b"recent");
}

#[tokio::test]
async fn gc_runs_again_after_interval() {
    let dir = CacheDir::new("gc-interval");
    let first = dir.put_blob(&fake_hex(0x01), b"old", 2 * HOUR);

    let _world = World::start(
        &dir,
        &[("CACHE_TTL", "1h"), ("CACHE_GC_INTERVAL", "1s")],
        blob_router(&fake_hex(0x01), Vec::new()),
    )
    .await;

    eventually("startup GC", || !first.exists()).await;
    let second = dir.put_blob(&fake_hex(0x02), b"old", 2 * HOUR);
    eventually("periodic GC", || !second.exists()).await;
}

/// GETs a blob aged `fresh_age` and then one aged `stale_age`; only the stale one may be touched.
async fn assert_hit_refresh(
    name: &str,
    vars: &[(&str, &str)],
    stale_age: Duration,
    fresh_age: Duration,
) {
    let dir = CacheDir::new(name);
    let stale_hex = fake_hex(0x0e);
    let fresh_hex = fake_hex(0x0f);
    let stale = dir.put_blob(&stale_hex, b"stale", stale_age);
    let fresh = dir.put_blob(&fresh_hex, b"fresh", fresh_age);
    let fresh_before = mtime(&fresh);
    let world = World::start(&dir, vars, blob_router(&stale_hex, Vec::new())).await;

    let requested_at = SystemTime::now() - Duration::from_secs(2);
    for (hex, content) in [(&fresh_hex, "fresh"), (&stale_hex, "stale")] {
        let response = world.proxy.get(&blob_path(hex)).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), content);
    }
    assert_eq!(world.hub.count(), 0);

    eventually("stale blob mtime to be refreshed", || {
        mtime(&stale) >= requested_at
    })
    .await;
    assert!(mtime(&stale) <= SystemTime::now() + Duration::from_secs(2));
    assert_eq!(mtime(&fresh), fresh_before);
}

#[tokio::test]
async fn hit_refreshes_mtime_after_one_hour_without_ttl() {
    assert_hit_refresh("refresh-default", &[], 2 * HOUR, 30 * MINUTE).await;
}

#[tokio::test]
async fn hit_refresh_throttle_is_a_tenth_of_ttl() {
    assert_hit_refresh(
        "refresh-ttl",
        &[("CACHE_TTL", "1h")],
        10 * MINUTE,
        3 * MINUTE,
    )
    .await;
}
