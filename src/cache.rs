//! Local disk cache for content-addressed blobs.

use std::{
    collections::HashMap,
    ffi::{CString, OsString},
    fs::{self, File},
    io,
    os::unix::{ffi::OsStrExt, fs::FileExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering::Relaxed},
    },
    time::{Duration, Instant, SystemTime},
};

use axum::body::{Body, Bytes};
use ring::digest;
use tokio::{sync::watch, task::spawn_blocking, time::MissedTickBehavior};
use tracing::{debug, info, warn};

use crate::config::{CacheConfig, Threshold};

const READ_CHUNK: u64 = 256 * 1024;

pub(crate) struct Cache {
    dir: PathBuf,
    blobs: PathBuf,
    tmp: PathBuf,
    threshold: Option<Threshold>,
    ttl: Option<Duration>,
    touch_after: Duration,
    /// Total size of the blobs under `blobs`, reset by every full scan.
    cached_bytes: AtomicU64,
    /// Held while publishing a blob and during a collection pass, so a pass that resets
    /// `cached_bytes` from its scan cannot lose a concurrent publish.
    publish: tokio::sync::Mutex<()>,
    pause: Mutex<Pause>,
    fills: Mutex<HashMap<String, watch::Sender<Progress>>>,
    next_tmp: AtomicU64,
}

#[derive(Default)]
struct Pause {
    paused: bool,
    /// Incremented by every pause, so a collection pass only lifts pauses it has seen.
    generation: u64,
}

/// An open blob file. Unix keeps the handle readable after the path is renamed or removed.
#[derive(Clone)]
struct Blob {
    file: Arc<File>,
    len: u64,
}

#[derive(Clone, Default)]
pub(crate) struct Progress {
    /// Set once the owner has started writing the temporary file.
    blob: Option<Blob>,
    written: u64,
    failed: bool,
}

/// Blob bytes that are on disk or still being downloaded.
pub(crate) struct Source {
    blob: Blob,
    progress: watch::Receiver<Progress>,
}

pub(crate) enum Join {
    /// No download is in flight; the caller fetches upstream and hands the body to [`Filler::start`].
    Owner(Filler),
    /// Another request is fetching the blob; wait for it with [`follow`].
    Follower(watch::Receiver<Progress>),
}

/// Exclusive right to download one digest. Dropping it before success marks the fill as failed.
pub(crate) struct Filler {
    cache: Arc<Cache>,
    hex: String,
    progress: watch::Sender<Progress>,
    tmp: Option<PathBuf>,
    finished: bool,
}

#[derive(Debug)]
struct Entry {
    name: OsString,
    size: u64,
    modified: SystemTime,
}

#[derive(Clone, Copy)]
struct Usage {
    used: u64,
    low: u64,
    high: u64,
}

pub(crate) enum Lookup {
    Hit(Source),
    Miss,
    /// Something other than a regular file occupies the blob's name; bypass the cache.
    Unusable,
}

/// Whether `value` is a SHA-256 digest in the only form used as a file name: 64 lowercase hex digits.
pub(crate) fn is_digest_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

impl Cache {
    /// Prepares `CACHE_DIR` and starts the periodic collection task. Requires a Tokio runtime.
    pub(crate) fn open(config: &CacheConfig) -> Result<Arc<Cache>, String> {
        let failed = |action: &str, err: io::Error| {
            format!(
                "CACHE_DIR {}: failed to {action}: {err}",
                config.dir.display()
            )
        };
        // tokio's interval panics on its first tick if the next deadline overflows `Instant`.
        if Instant::now().checked_add(config.gc_interval).is_none() {
            return Err(format!(
                "CACHE_GC_INTERVAL {:?} is too large",
                config.gc_interval
            ));
        }
        let blobs = config.dir.join("sha256");
        let tmp = config.dir.join("tmp");
        for dir in [&blobs, &tmp] {
            fs::create_dir_all(dir).map_err(|err| failed("create directories", err))?;
            // Startup clears `tmp/` and collection deletes from `sha256/`; never follow a link out.
            if !fs::symlink_metadata(dir).is_ok_and(|metadata| metadata.is_dir()) {
                return Err(format!(
                    "{} must be a directory, not a symlink",
                    dir.display()
                ));
            }
        }
        clear_dir(&tmp).map_err(|err| failed("clear tmp", err))?;
        // Publishing renames from `tmp/` into `sha256/`, so probe exactly that.
        let probe = tmp.join("probe");
        let published = blobs.join("probe");
        File::create_new(&probe)
            .and_then(|_| fs::rename(&probe, &published))
            .and_then(|_| fs::remove_file(&published))
            .map_err(|err| failed("write", err))?;

        let cache = Arc::new(Cache {
            dir: config.dir.clone(),
            blobs,
            tmp,
            threshold: config.threshold,
            ttl: config.ttl,
            touch_after: touch_interval(config.ttl),
            cached_bytes: AtomicU64::new(0),
            publish: tokio::sync::Mutex::new(()),
            pause: Mutex::new(Pause::default()),
            fills: Mutex::new(HashMap::new()),
            next_tmp: AtomicU64::new(0),
        });
        let entries = cache.scan(true).map_err(|err| failed("scan", err))?;
        cache
            .cached_bytes
            .store(entries.iter().map(|entry| entry.size).sum(), Relaxed);
        if cache.threshold.is_none() && cache.ttl.is_none() {
            warn!(
                "neither CACHE_GC_THRESHOLD nor CACHE_TTL is set, cached blobs are never removed"
            );
        }
        tokio::spawn(collect_periodically(cache.clone(), config.gc_interval));
        Ok(cache)
    }

    pub(crate) fn is_paused(&self) -> bool {
        self.pause.lock().unwrap().paused
    }

    /// Opens a cached blob, refreshing its mtime when it is older than the touch interval.
    pub(crate) async fn lookup(self: &Arc<Self>, hex: &str) -> Lookup {
        let cache = self.clone();
        let path = self.blobs.join(hex);
        spawn_blocking(move || cache.open_blob(&path))
            .await
            .unwrap_or(Lookup::Unusable)
    }

    fn open_blob(&self, path: &Path) -> Lookup {
        let file = match File::open(path) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Lookup::Miss,
            Err(_) => return Lookup::Unusable,
        };
        let Ok(metadata) = file.metadata().inspect_err(|err| {
            debug!(path = %path.display(), "failed to stat cached blob: {err}");
        }) else {
            return Lookup::Unusable;
        };
        if !metadata.is_file() {
            return Lookup::Unusable;
        }
        let now = SystemTime::now();
        let stale = metadata.modified().is_ok_and(|modified| {
            now.duration_since(modified)
                .is_ok_and(|age| age > self.touch_after)
        });
        if stale && let Err(err) = file.set_modified(now) {
            debug!(path = %path.display(), "failed to refresh cache mtime: {err}");
        }
        Lookup::Hit(Source::complete(Blob {
            file: Arc::new(file),
            len: metadata.len(),
        }))
    }

    pub(crate) fn join(self: &Arc<Self>, hex: &str) -> Join {
        let mut fills = self.fills.lock().unwrap();
        if let Some(progress) = fills.get(hex) {
            return Join::Follower(progress.subscribe());
        }
        let (progress, _) = watch::channel(Progress::default());
        fills.insert(hex.to_string(), progress.clone());
        Join::Owner(Filler {
            cache: self.clone(),
            hex: hex.to_string(),
            progress,
            tmp: None,
            finished: false,
        })
    }

    fn pause(&self, reason: &str) {
        let mut pause = self.pause.lock().unwrap();
        pause.generation += 1;
        if !pause.paused {
            pause.paused = true;
            warn!("cache writes paused: {reason}");
        }
    }

    fn pause_generation(&self) -> u64 {
        self.pause.lock().unwrap().generation
    }

    /// Lifts the pause unless something paused writes again after `seen` was read.
    fn resume(&self, seen: u64) {
        let mut pause = self.pause.lock().unwrap();
        if pause.paused && pause.generation == seen {
            pause.paused = false;
            warn!("cache writes resumed");
        }
    }

    /// Cached blobs sorted by mtime, oldest first. Unexpected entries are skipped.
    fn scan(&self, warn_unexpected: bool) -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for item in fs::read_dir(&self.blobs)? {
            let item = item?;
            let name = item.file_name();
            let expected = name.to_str().is_some_and(is_digest_hex)
                && item.file_type().is_ok_and(|kind| kind.is_file());
            if !expected {
                if warn_unexpected {
                    warn!(path = %item.path().display(), "ignoring unexpected entry in cache directory");
                }
                continue;
            }
            let Ok(metadata) = item.metadata() else {
                continue;
            };
            let Ok(modified) = metadata.modified() else {
                continue;
            };
            entries.push(Entry {
                name,
                size: metadata.len(),
                modified,
            });
        }
        entries.sort_by_key(|entry| entry.modified);
        Ok(entries)
    }

    fn usage(&self, threshold: Threshold) -> io::Result<Usage> {
        Ok(match threshold {
            Threshold::Bytes { low, high } => Usage {
                used: self.cached_bytes.load(Relaxed),
                low,
                high,
            },
            Threshold::Percent { low, high } => {
                let (used, total) = disk_usage(&self.dir)?;
                let share = |percent: f64| (total as f64 * percent / 100.0) as u64;
                Usage {
                    used,
                    low: share(low),
                    high: share(high),
                }
            }
        })
    }

    fn measure(&self) -> Option<Usage> {
        let threshold = self.threshold?;
        self.usage(threshold)
            .inspect_err(|err| warn!("failed to measure cache usage: {err}"))
            .ok()
    }

    /// One collection pass: TTL expiry, then LRU down to the low watermark when above the high
    /// one. Afterwards resumes paused writes, or pauses them when removing every blob was not
    /// enough to get below the high watermark.
    fn collect(&self) {
        let seen = self.pause_generation();
        let _publish = self.publish.blocking_lock();
        let usage = self.measure();
        let need = usage
            .filter(|usage| usage.used > usage.high)
            .map_or(0, |usage| usage.used - usage.low);
        let emptied = (need > 0 || self.ttl.is_some()) && self.remove(need);
        let usage = if need > 0 { self.measure() } else { usage };
        match usage {
            // Only reachable with `need > 0`: usage at or below the high watermark needs nothing.
            Some(usage) if usage.used > usage.high => {
                if emptied {
                    self.pause("usage stays above the high watermark with no cached blobs left");
                }
            }
            Some(_) => self.resume(seen),
            None if self.threshold.is_none() => self.resume(seen),
            None => {}
        }
    }

    /// Removes expired blobs and then the oldest ones until `need` bytes are freed.
    /// Returns whether every cached blob was removed.
    fn remove(&self, need: u64) -> bool {
        let entries = match self.scan(false) {
            Ok(entries) => entries,
            Err(err) => {
                warn!("failed to scan cache directory: {err}");
                return false;
            }
        };
        let total = entries.iter().map(|entry| entry.size).sum::<u64>();
        // The counter drifts when blobs are deleted by hand; in size mode trust the scan instead.
        let need = match self.threshold {
            Some(Threshold::Bytes { low, high }) if total > high => total - low,
            Some(Threshold::Bytes { .. }) => 0,
            _ => need,
        };
        let cutoff = self.ttl.and_then(|ttl| SystemTime::now().checked_sub(ttl));
        let count = victims(&entries, cutoff, need);
        let (mut removed, mut freed) = (0, 0);
        for entry in &entries[..count] {
            match fs::remove_file(self.blobs.join(&entry.name)) {
                Ok(()) => {
                    removed += 1;
                    freed += entry.size;
                }
                Err(err) => warn!(blob = ?entry.name, "failed to remove cached blob: {err}"),
            }
        }
        let cached = total - freed;
        self.cached_bytes.store(cached, Relaxed);
        if removed > 0 {
            info!(removed, freed, cached, "cache collection removed blobs");
        }
        removed == entries.len()
    }
}

impl Filler {
    /// Creates the temporary file and downloads `upstream` into it in the background.
    /// Returns the upstream response back when the file cannot be created.
    #[allow(clippy::result_large_err)] // the caller streams the returned response, so it must not be dropped
    pub(crate) fn start(
        mut self,
        len: u64,
        upstream: reqwest::Response,
    ) -> Result<Source, reqwest::Response> {
        // Writes may have been paused while this request waited for the upstream headers.
        if self.cache.is_paused() {
            return Err(upstream);
        }
        let index = self.cache.next_tmp.fetch_add(1, Relaxed);
        let tmp = self.cache.tmp.join(format!("{}-{index}", self.hex));
        let file = match File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(err) => {
                self.cache
                    .pause(&format!("failed to create {}: {err}", tmp.display()));
                return Err(upstream);
            }
        };
        self.tmp = Some(tmp);
        let blob = Blob {
            file: Arc::new(file),
            len,
        };
        self.progress
            .send_modify(|progress| progress.blob = Some(blob.clone()));
        let source = Source {
            blob: blob.clone(),
            progress: self.progress.subscribe(),
        };
        tokio::spawn(self.download(upstream, blob));
        Ok(source)
    }

    /// Ends the fill with a blob that another fill published meanwhile, handing it to the followers.
    pub(crate) fn complete(mut self, source: &Source) {
        self.progress.send_modify(|progress| {
            progress.blob = Some(source.blob.clone());
            progress.written = source.blob.len;
        });
        self.finish(true);
    }

    async fn download(mut self, mut upstream: reqwest::Response, blob: Blob) {
        let mut hasher = digest::Context::new(&digest::SHA256);
        let mut written = 0;
        let complete = loop {
            let next = tokio::select! {
                biased;
                // Checked first: a fast upstream always has a chunk ready and would starve it.
                () = self.progress.closed(), if written < blob.len => {
                    debug!(digest = self.hex, "every client disconnected, abandoning blob download");
                    break false;
                }
                next = upstream.chunk() => next,
            };
            let chunk = match next {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break true,
                Err(err) => {
                    debug!(digest = self.hex, "blob download failed: {err}");
                    break false;
                }
            };
            let size = chunk.len() as u64;
            if written + size > blob.len {
                break false;
            }
            hasher.update(&chunk);
            let file = blob.file.clone();
            let offset = written;
            if let Err(err) = blocking(move || file.write_all_at(&chunk, offset)).await {
                self.cache
                    .pause(&format!("failed to write cache file: {err}"));
                break false;
            }
            written += size;
            self.progress
                .send_modify(|progress| progress.written = written);
        };
        if !complete || written != blob.len {
            return;
        }
        if hex(hasher.finish().as_ref()) != self.hex {
            warn!(
                digest = self.hex,
                "upstream blob does not match its digest, not caching it"
            );
            return;
        }
        let tmp = self.tmp.clone().expect("started fill has a temporary file");
        let published = async {
            let file = blob.file.clone();
            blocking(move || file.sync_all()).await?;
            // Wait without holding a blocking thread while a collection pass runs.
            let _publish = self.cache.publish.lock().await;
            let target = self.cache.blobs.join(&self.hex);
            blocking(move || fs::rename(&tmp, &target)).await?;
            self.cache.cached_bytes.fetch_add(blob.len, Relaxed);
            io::Result::Ok(())
        }
        .await;
        match published {
            Ok(()) => {
                self.tmp = None;
                self.finish(true);
            }
            Err(err) => self
                .cache
                .pause(&format!("failed to publish cache file: {err}")),
        }
    }

    fn finish(&mut self, success: bool) {
        self.finished = true;
        if !success {
            self.progress.send_modify(|progress| progress.failed = true);
            if let Some(tmp) = &self.tmp
                && let Err(err) = fs::remove_file(tmp)
                && err.kind() != io::ErrorKind::NotFound
            {
                warn!(path = %tmp.display(), "failed to remove temporary cache file: {err}");
            }
        }
        self.cache.fills.lock().unwrap().remove(&self.hex);
    }
}

impl Drop for Filler {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(false);
        }
    }
}

/// Waits until the owner of a fill has started its download (`Some`) or given up (`None`).
pub(crate) async fn follow(mut progress: watch::Receiver<Progress>) -> Option<Source> {
    loop {
        let (blob, failed) = {
            let current = progress.borrow_and_update();
            (current.blob.clone(), current.failed)
        };
        if failed {
            return None;
        }
        if let Some(blob) = blob {
            return Some(Source { blob, progress });
        }
        progress.changed().await.ok()?;
    }
}

impl Source {
    fn complete(blob: Blob) -> Source {
        let (_, progress) = watch::channel(Progress {
            blob: Some(blob.clone()),
            written: blob.len,
            failed: false,
        });
        Source { blob, progress }
    }

    pub(crate) fn len(&self) -> u64 {
        self.blob.len
    }

    /// Streams the blob, waiting for bytes that are still being downloaded.
    /// Ends with an error when the download fails before all bytes are written.
    pub(crate) fn into_body(self) -> Body {
        let stream = futures_util::stream::unfold(Some((self, 0)), |state| async move {
            let (mut source, offset) = state?;
            loop {
                if offset == source.blob.len {
                    return None;
                }
                let (written, failed) = {
                    let progress = source.progress.borrow_and_update();
                    (progress.written, progress.failed)
                };
                if offset < written {
                    let size = (written - offset).min(READ_CHUNK);
                    let file = source.blob.file.clone();
                    let read = blocking(move || {
                        let mut buf = vec![0; size as usize];
                        file.read_exact_at(&mut buf, offset)
                            .map(|()| Bytes::from(buf))
                    })
                    .await;
                    return Some(match read {
                        Ok(bytes) => (Ok(bytes), Some((source, offset + size))),
                        Err(err) => (Err(err), None),
                    });
                }
                if failed || source.progress.changed().await.is_err() {
                    return Some((Err(io::Error::other("blob download failed")), None));
                }
            }
        });
        Body::from_stream(stream)
    }
}

async fn collect_periodically(cache: Arc<Cache>, period: Duration) {
    let mut ticks = tokio::time::interval(period);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticks.tick().await;
        let cache = cache.clone();
        if let Err(err) = spawn_blocking(move || cache.collect()).await {
            warn!("cache collection failed: {err}");
        }
    }
}

/// How many of `entries` (sorted oldest first) to remove: every entry older than `cutoff`,
/// and then more until at least `need` bytes are freed.
fn victims(entries: &[Entry], cutoff: Option<SystemTime>, need: u64) -> usize {
    let mut freed = 0;
    entries
        .iter()
        .take_while(|entry| {
            let take = cutoff.is_some_and(|cutoff| entry.modified < cutoff) || freed < need;
            if take {
                freed += entry.size;
            }
            take
        })
        .count()
}

/// A tenth of the TTL within one minute to one hour; one hour without a TTL.
fn touch_interval(ttl: Option<Duration>) -> Duration {
    const MIN: Duration = Duration::from_secs(60);
    const MAX: Duration = Duration::from_secs(3600);
    ttl.map_or(MAX, |ttl| (ttl / 10).clamp(MIN, MAX))
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    spawn_blocking(work)
        .await
        .unwrap_or_else(|err| Err(io::Error::other(err)))
}

fn clear_dir(dir: &Path) -> io::Result<()> {
    for item in fs::read_dir(dir)? {
        let item = item?;
        let path = item.path();
        if item.file_type()?.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Used and total bytes of the file system holding `path`, counting reserved blocks as used.
#[allow(clippy::useless_conversion)] // field widths differ between targets
fn disk_usage(path: &Path) -> io::Result<(u64, u64)> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: `path` is NUL-terminated and `stat` points to writable memory of the right type.
    if unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statvfs returned 0, so it filled in `stat`.
    let stat = unsafe { stat.assume_init() };
    let block = u64::from(stat.f_frsize);
    let total = u64::from(stat.f_blocks) * block;
    let available = u64::from(stat.f_bavail) * block;
    Ok((total.saturating_sub(available), total))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(spec: &[(u64, u64)]) -> Vec<Entry> {
        spec.iter()
            .map(|&(age_secs, size)| Entry {
                name: OsString::from(format!("{age_secs}")),
                size,
                modified: SystemTime::UNIX_EPOCH + Duration::from_secs(10_000 - age_secs),
            })
            .collect()
    }

    fn ago(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(10_000 - secs))
    }

    #[test]
    fn selects_oldest_entries() {
        let list = entries(&[(400, 10), (300, 10), (200, 10), (100, 10)]);
        assert_eq!(victims(&list, None, 0), 0);
        assert_eq!(victims(&list, None, 1), 1);
        assert_eq!(victims(&list, None, 10), 1);
        assert_eq!(victims(&list, None, 11), 2);
        assert_eq!(victims(&list, None, 1000), 4);
        assert_eq!(victims(&list, ago(250), 0), 2);
        assert_eq!(victims(&list, ago(250), 15), 2);
        assert_eq!(victims(&list, ago(250), 25), 3);
        assert_eq!(victims(&list, ago(450), 15), 2);
        assert_eq!(victims(&list, ago(400), 0), 0);
        assert_eq!(victims(&[], ago(1), 100), 0);
    }

    #[test]
    fn touch_interval_follows_ttl() {
        let secs = Duration::from_secs;
        assert_eq!(touch_interval(None), secs(3600));
        assert_eq!(touch_interval(Some(secs(7 * 86400))), secs(3600));
        assert_eq!(touch_interval(Some(secs(5 * 3600))), secs(1800));
        assert_eq!(touch_interval(Some(secs(300))), secs(60));
    }

    #[test]
    fn validates_digest_hex() {
        assert!(is_digest_hex(&"0123456789abcdef".repeat(4)));
        assert!(!is_digest_hex(&"0123456789ABCDEF".repeat(4)));
        assert!(!is_digest_hex(&"a".repeat(63)));
        assert!(!is_digest_hex(&"a".repeat(65)));
        assert!(!is_digest_hex(&format!("{}g", "a".repeat(63))));
    }

    #[test]
    fn encodes_hex() {
        assert_eq!(hex(&[0x00, 0x0f, 0xa0, 0xff]), "000fa0ff");
    }

    #[test]
    fn measures_disk_usage() {
        let (used, total) = disk_usage(Path::new("/")).unwrap();
        assert!(total > 0 && used <= total);
        assert!(disk_usage(Path::new("/nonexistent/path")).is_err());
    }
}
