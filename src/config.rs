use std::collections::BTreeMap;

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
pub struct Config {
    pub port: u16,
    pub mode: Mode,
    pub upstream: String,
    pub routes: BTreeMap<String, String>,
    pub blocked_ua: Vec<String>,
    pub auth_url: String,
    pub auth: AuthMode,
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

        Ok(Config {
            port,
            mode,
            upstream,
            routes,
            blocked_ua,
            auth_url,
            auth,
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
