use std::{collections::BTreeMap, path::PathBuf, time::Duration};

pub const DOCKER_HUB: &str = "registry-1.docker.io";

const DEFAULT_ROUTES: &[(&str, &str)] = &[
    ("quay", "quay.io"),
    ("gcr", "gcr.io"),
    ("k8s-gcr", "k8s.gcr.io"),
    ("k8s", "registry.k8s.io"),
    ("ghcr", "ghcr.io"),
    ("cloudsmith", "docker.cloudsmith.io"),
    ("nvcr", "nvcr.io"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Single,
    Route,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Single => "single",
            Mode::Route => "route",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthMode {
    Passthrough,
    Basic { user: String, pass: String },
}

impl AuthMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthMode::Passthrough => "passthrough",
            AuthMode::Basic { .. } => "basic",
        }
    }
}

#[derive(Clone, Debug)]
pub struct CacheConfig {
    pub dir: PathBuf,
    pub threshold: Option<Threshold>,
    pub ttl: Option<Duration>,
    pub gc_interval: Duration,
}

/// `CACHE_GC_THRESHOLD`: collection starts above `high` and removes blobs down to `low`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Threshold {
    /// Total size of the cached blobs, in bytes.
    Bytes { low: u64, high: u64 },
    /// Usage of the file system holding `CACHE_DIR`, in percent.
    Percent { low: f64, high: f64 },
}

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    pub mode: Mode,
    pub upstream: String,
    pub routes: BTreeMap<String, String>,
    pub blocked_ua: Vec<String>,
    pub auth_url: String,
    pub auth: AuthMode,
    pub cache: Option<CacheConfig>,
    /// Scheme used to reach upstream registries. Always `https` in production;
    /// integration tests switch it to `http` to talk to local mock servers.
    pub upstream_scheme: String,
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Builds the configuration from a variable lookup. Empty values count as unset,
    /// matching the `process.env.X || default` semantics of the original service.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let var = |name: &str| get(name).filter(|value| !value.is_empty());

        let port = match var("PORT") {
            Some(raw) => raw
                .trim()
                .parse::<u16>()
                .map_err(|_| format!("invalid PORT {raw:?}"))?,
            None => 3000,
        };

        let mode = match var("MODE").map(|m| m.to_ascii_lowercase()).as_deref() {
            None | Some("single") => Mode::Single,
            Some("route") => Mode::Route,
            Some(other) => return Err(format!("invalid MODE {other:?}, expected single or route")),
        };

        let upstream = var("UPSTREAM").unwrap_or_else(|| DOCKER_HUB.to_string());
        validate_host("UPSTREAM", &upstream)?;

        let mut routes: BTreeMap<String, String> = DEFAULT_ROUTES
            .iter()
            .map(|(prefix, host)| (prefix.to_string(), host.to_string()))
            .collect();
        if let Some(raw) = var("ROUTES") {
            let value: serde_json::Value = serde_json::from_str(&raw)
                .map_err(|err| format!("failed to parse ROUTES: {err}"))?;
            let object = value
                .as_object()
                .ok_or_else(|| "ROUTES must be a JSON object".to_string())?;
            for (prefix, host) in object {
                let host = host
                    .as_str()
                    .ok_or_else(|| format!("ROUTES[{prefix:?}] must be a string"))?;
                validate_host(&format!("ROUTES[{prefix:?}]"), host)?;
                routes.insert(prefix.to_ascii_lowercase(), host.to_string());
            }
        }

        let blocked_ua = var("BLOCKED_UA")
            .unwrap_or_else(|| "netcraft".to_string())
            .split(|c: char| c.is_whitespace() || c == ',' || c == '|')
            .filter(|item| !item.is_empty())
            .map(|item| item.to_lowercase())
            .collect();

        let auth_url = var("AUTH_URL")
            .unwrap_or_else(|| "https://auth.docker.io".to_string())
            .trim_end_matches('/')
            .to_string();
        match reqwest::Url::parse(&auth_url) {
            Ok(url) if matches!(url.scheme(), "http" | "https") && url.host_str().is_some() => {}
            _ => return Err(format!("invalid AUTH_URL {auth_url:?}")),
        }

        let auth = match var("AUTH_MODE").map(|m| m.to_ascii_lowercase()).as_deref() {
            None | Some("passthrough") => AuthMode::Passthrough,
            Some("basic") => match (var("AUTH_USER"), var("AUTH_PASS")) {
                (Some(user), Some(pass)) => AuthMode::Basic { user, pass },
                _ => {
                    return Err("AUTH_MODE=basic but AUTH_USER or AUTH_PASS is not set".to_string());
                }
            },
            Some(other) => {
                return Err(format!(
                    "invalid AUTH_MODE {other:?}, expected passthrough or basic"
                ));
            }
        };

        let cache = match var("CACHE_DIR") {
            None => None,
            Some(dir) => Some(CacheConfig {
                dir: PathBuf::from(dir),
                threshold: var("CACHE_GC_THRESHOLD")
                    .map(|raw| parse_threshold(&raw))
                    .transpose()?,
                ttl: var("CACHE_TTL")
                    .map(|raw| parse_duration("CACHE_TTL", &raw))
                    .transpose()?,
                gc_interval: match var("CACHE_GC_INTERVAL") {
                    Some(raw) => parse_duration("CACHE_GC_INTERVAL", &raw)?,
                    None => Duration::from_secs(300),
                },
            }),
        };

        Ok(Config {
            port,
            mode,
            upstream,
            routes,
            blocked_ua,
            auth_url,
            auth,
            cache,
            upstream_scheme: "https".to_string(),
        })
    }

    /// Picks the upstream registry for a request.
    ///
    /// Order: `ns` query parameter, then (route mode only) the first label of the
    /// `hubhost` query parameter or the request host, then `UPSTREAM`.
    /// Returns `Err(ns)` when `ns` names a registry outside the configured set.
    pub fn resolve_upstream(
        &self,
        query: Option<&str>,
        host: Option<&str>,
    ) -> Result<String, String> {
        let mut ns = None;
        let mut hubhost = None;
        for (key, value) in form_urlencoded::parse(query.unwrap_or("").as_bytes()) {
            match key.as_ref() {
                "ns" if ns.is_none() => ns = Some(value.into_owned()),
                "hubhost" if hubhost.is_none() => hubhost = Some(value.into_owned()),
                _ => {}
            }
        }

        if let Some(ns) = ns.filter(|value| !value.is_empty()) {
            let target = if ns.eq_ignore_ascii_case("docker.io") {
                DOCKER_HUB.to_string()
            } else {
                ns.to_ascii_lowercase()
            };
            return if self.is_allowed_upstream(&target) {
                Ok(target)
            } else {
                Err(ns)
            };
        }

        if self.mode == Mode::Route {
            let hostname = match hubhost.filter(|value| !value.is_empty()) {
                Some(value) => value,
                None => hostname(host.unwrap_or("")).to_string(),
            };
            let prefix = hostname
                .split('.')
                .next()
                .unwrap_or("")
                .to_ascii_lowercase();
            if let Some(target) = self.routes.get(&prefix) {
                return Ok(target.clone());
            }
        }

        Ok(self.upstream.clone())
    }

    pub fn is_allowed_upstream(&self, host: &str) -> bool {
        host.eq_ignore_ascii_case(DOCKER_HUB)
            || host.eq_ignore_ascii_case(&self.upstream)
            || self
                .routes
                .values()
                .any(|route| route.eq_ignore_ascii_case(host))
    }
}

fn validate_host(name: &str, value: &str) -> Result<(), String> {
    if value.contains('/') || value.chars().any(char::is_whitespace) {
        return Err(format!(
            "{name} must be a registry host such as quay.io, got {value:?}"
        ));
    }
    Ok(())
}

/// `low:high` or a single value used for both; both sides are sizes or both are percentages.
fn parse_threshold(raw: &str) -> Result<Threshold, String> {
    let invalid = || format!("invalid CACHE_GC_THRESHOLD {raw:?}");
    let (low, high) = raw.split_once(':').unwrap_or((raw, raw));
    let threshold = match (low.strip_suffix('%'), high.strip_suffix('%')) {
        (Some(low), Some(high)) => Threshold::Percent {
            low: parse_percent(low).ok_or_else(invalid)?,
            high: parse_percent(high).ok_or_else(invalid)?,
        },
        (None, None) => Threshold::Bytes {
            low: parse_size(low).ok_or_else(invalid)?,
            high: parse_size(high).ok_or_else(invalid)?,
        },
        _ => return Err(invalid()),
    };
    let ordered = match threshold {
        Threshold::Bytes { low, high } => low <= high,
        Threshold::Percent { low, high } => low <= high,
    };
    if ordered {
        Ok(threshold)
    } else {
        Err(invalid())
    }
}

fn parse_percent(raw: &str) -> Option<f64> {
    raw.parse::<f64>()
        .ok()
        .filter(|value| (0.0..=100.0).contains(value))
}

/// A positive byte count, optionally with a `Ki`, `Mi`, `Gi` or `Ti` suffix.
fn parse_size(raw: &str) -> Option<u64> {
    let split = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (digits, unit) = raw.split_at(split);
    let shift = match unit {
        "" => 0,
        "Ki" => 10,
        "Mi" => 20,
        "Gi" => 30,
        "Ti" => 40,
        _ => return None,
    };
    digits
        .parse::<u64>()
        .ok()?
        .checked_mul(1 << shift)
        .filter(|&size| size > 0)
}

/// A positive integer followed by one of `s`, `m`, `h` or `d`.
fn parse_duration(name: &str, raw: &str) -> Result<Duration, String> {
    let invalid = || format!("invalid {name} {raw:?}, expected e.g. 30s, 5m, 12h or 7d");
    let unit_secs = match raw.bytes().last() {
        Some(b's') => 1,
        Some(b'm') => 60,
        Some(b'h') => 3600,
        Some(b'd') => 86400,
        _ => return Err(invalid()),
    };
    let digits = &raw[..raw.len() - 1];
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    digits
        .parse::<u64>()
        .ok()
        .and_then(|count| count.checked_mul(unit_secs))
        .filter(|&secs| secs > 0)
        .map(Duration::from_secs)
        .ok_or_else(invalid)
}

/// Strips the port from a Host header value, keeping bracketed IPv6 literals intact.
fn hostname(host: &str) -> &str {
    match host.rfind(':') {
        Some(index) if !host[index..].contains(']') => &host[..index],
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(vars: &[(&str, &str)]) -> Result<Config, String> {
        Config::from_lookup(|name| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.to_string())
        })
    }

    #[test]
    fn defaults_match_the_original_service() {
        let config = config(&[]).unwrap();
        assert_eq!(config.port, 3000);
        assert_eq!(config.mode, Mode::Single);
        assert_eq!(config.upstream, DOCKER_HUB);
        assert_eq!(config.auth_url, "https://auth.docker.io");
        assert_eq!(config.auth, AuthMode::Passthrough);
        assert_eq!(config.blocked_ua, vec!["netcraft"]);
        assert_eq!(config.routes.len(), DEFAULT_ROUTES.len());
        assert_eq!(config.upstream_scheme, "https");
        assert!(config.cache.is_none());
    }

    #[test]
    fn parses_cache_settings() {
        let cache = config(&[("CACHE_DIR", "/var/cache/registry")])
            .unwrap()
            .cache
            .unwrap();
        assert_eq!(cache.dir, PathBuf::from("/var/cache/registry"));
        assert_eq!(cache.threshold, None);
        assert_eq!(cache.ttl, None);
        assert_eq!(cache.gc_interval, Duration::from_secs(300));

        let cache = config(&[
            ("CACHE_DIR", "/cache"),
            ("CACHE_GC_THRESHOLD", "80Gi:100Gi"),
            ("CACHE_TTL", "7d"),
            ("CACHE_GC_INTERVAL", "30s"),
        ])
        .unwrap()
        .cache
        .unwrap();
        assert_eq!(
            cache.threshold,
            Some(Threshold::Bytes {
                low: 80 << 30,
                high: 100 << 30
            })
        );
        assert_eq!(cache.ttl, Some(Duration::from_secs(7 * 86400)));
        assert_eq!(cache.gc_interval, Duration::from_secs(30));

        assert!(config(&[("CACHE_TTL", "bogus")]).unwrap().cache.is_none());
    }

    #[test]
    fn parses_gc_thresholds() {
        for (raw, expected) in [
            (
                "1000",
                Threshold::Bytes {
                    low: 1000,
                    high: 1000,
                },
            ),
            (
                "2Ki:1Mi",
                Threshold::Bytes {
                    low: 2048,
                    high: 1 << 20,
                },
            ),
            (
                "1Ti",
                Threshold::Bytes {
                    low: 1 << 40,
                    high: 1 << 40,
                },
            ),
            (
                "90%:95%",
                Threshold::Percent {
                    low: 90.0,
                    high: 95.0,
                },
            ),
            (
                "92.5%",
                Threshold::Percent {
                    low: 92.5,
                    high: 92.5,
                },
            ),
            (
                "0%:100%",
                Threshold::Percent {
                    low: 0.0,
                    high: 100.0,
                },
            ),
        ] {
            assert_eq!(parse_threshold(raw), Ok(expected), "{raw}");
        }
        for raw in [
            "",
            "0",
            "0:10",
            "10:0",
            "95%:90%",
            "90%:100Gi",
            "100Gi:95%",
            "101%",
            "-1%",
            "nan%",
            "10G",
            "10gi",
            "1.5Gi",
            "+10",
            " 10",
            "10:",
            ":10",
            "1:2:3",
            "abc",
            "1000:abc",
        ] {
            assert!(parse_threshold(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn parses_durations() {
        for (raw, secs) in [("30s", 30), ("5m", 300), ("12h", 43200), ("7d", 604_800)] {
            assert_eq!(
                parse_duration("X", raw),
                Ok(Duration::from_secs(secs)),
                "{raw}"
            );
        }
        for raw in [
            "", "s", "h", "0s", "0d", "10", "1w", "abc", "1h30m", "1.5h", "+5m", "-5m", "5 m",
            "5M", "5日",
        ] {
            assert!(parse_duration("X", raw).is_err(), "{raw}");
        }
        assert!(parse_duration("X", "99999999999999999999d").is_err());
        for vars in [
            &[("CACHE_DIR", "/c"), ("CACHE_TTL", "0s")][..],
            &[("CACHE_DIR", "/c"), ("CACHE_GC_INTERVAL", "1h30m")],
            &[("CACHE_DIR", "/c"), ("CACHE_GC_THRESHOLD", "95%:90%")],
        ] {
            assert!(config(vars).is_err(), "expected error for {vars:?}");
        }
    }

    #[test]
    fn empty_values_fall_back_to_defaults() {
        let config = config(&[("PORT", ""), ("BLOCKED_UA", ""), ("MODE", "")]).unwrap();
        assert_eq!(config.port, 3000);
        assert_eq!(config.blocked_ua, vec!["netcraft"]);
        assert_eq!(config.mode, Mode::Single);
    }

    #[test]
    fn parses_lists_routes_and_auth() {
        let config = config(&[
            ("BLOCKED_UA", "Foo, bar|baz  qux"),
            (
                "ROUTES",
                r#"{"Mirror":"mirror.example.com","quay":"quay.example.com"}"#,
            ),
            ("AUTH_MODE", "BASIC"),
            ("AUTH_USER", "user"),
            ("AUTH_PASS", "pa ss"),
            ("AUTH_URL", "https://auth.example.com/"),
        ])
        .unwrap();
        assert_eq!(config.blocked_ua, vec!["foo", "bar", "baz", "qux"]);
        assert_eq!(config.routes["mirror"], "mirror.example.com");
        assert_eq!(config.routes["quay"], "quay.example.com");
        assert_eq!(config.routes["ghcr"], "ghcr.io");
        assert_eq!(
            config.auth,
            AuthMode::Basic {
                user: "user".into(),
                pass: "pa ss".into()
            }
        );
        assert_eq!(config.auth_url, "https://auth.example.com");
    }

    #[test]
    fn rejects_invalid_values() {
        for vars in [
            &[("PORT", "abc")][..],
            &[("MODE", "mirror")],
            &[("AUTH_MODE", "token")],
            &[("AUTH_MODE", "basic"), ("AUTH_USER", "user")],
            &[("ROUTES", "not json")],
            &[("ROUTES", r#"["quay.io"]"#)],
            &[("ROUTES", r#"{"quay":1}"#)],
            &[("UPSTREAM", "https://quay.io")],
            &[("AUTH_URL", "auth.docker.io")],
        ] {
            assert!(config(vars).is_err(), "expected error for {vars:?}");
        }
    }

    #[test]
    fn resolves_upstream_from_ns_with_allowlist() {
        let config = config(&[]).unwrap();
        assert_eq!(
            config.resolve_upstream(Some("ns=docker.io"), None),
            Ok(DOCKER_HUB.into())
        );
        assert_eq!(
            config.resolve_upstream(Some("ns=Quay.io"), None),
            Ok("quay.io".into())
        );
        assert_eq!(
            config.resolve_upstream(Some("ns=evil.example.com"), None),
            Err("evil.example.com".into())
        );
        assert_eq!(
            config.resolve_upstream(Some("ns="), None),
            Ok(DOCKER_HUB.into())
        );
    }

    #[test]
    fn resolves_upstream_by_prefix_only_in_route_mode() {
        let single = config(&[("UPSTREAM", "ghcr.io")]).unwrap();
        assert_eq!(
            single.resolve_upstream(None, Some("quay.example.com")),
            Ok("ghcr.io".into())
        );

        let route = config(&[("MODE", "route")]).unwrap();
        assert_eq!(
            route.resolve_upstream(None, Some("quay.example.com:8443")),
            Ok("quay.io".into())
        );
        assert_eq!(
            route.resolve_upstream(None, Some("k8s-gcr.example.com")),
            Ok("k8s.gcr.io".into())
        );
        assert_eq!(
            route.resolve_upstream(None, Some("docker.example.com")),
            Ok(DOCKER_HUB.into())
        );
        assert_eq!(
            route.resolve_upstream(Some("hubhost=ghcr.example.com"), Some("docker.example.com")),
            Ok("ghcr.io".into())
        );
        assert_eq!(
            route.resolve_upstream(None, Some("[::1]:3000")),
            Ok(DOCKER_HUB.into())
        );
        assert_eq!(route.resolve_upstream(None, None), Ok(DOCKER_HUB.into()));
    }
}
