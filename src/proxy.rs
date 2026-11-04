//! Outgoing connections through a proxy (#26, #74; #21, #59, #151 are users behind one).
//!
//! Every client gdrive builds (the Drive hub and the OAuth token client of the login, the
//! refresh, service accounts and Application Default Credentials) connects through
//! [`ProxyConnector`], under the rustls connector of [`crate::hub`]. So TLS to Google runs end to
//! end through the proxy tunnel, with HTTP/2 as without a proxy.
//!
//! The proxy comes from the environment, as curl and most tools read it: `HTTPS_PROXY` for
//! https:// destinations, `HTTP_PROXY` for http:// ones, `ALL_PROXY` for both when the specific
//! one is not set, and `NO_PROXY` for the hosts to reach directly; the lowercase names work too,
//! the uppercase name wins when both are set. hyper-util's `Matcher` applies these rules
//! (including `NO_PROXY` domains, addresses and ranges), its `Tunnel` connector opens HTTP
//! CONNECT tunnels and its `SocksV5` connector SOCKS5 ones:
//!
//! - `http://[user:password@]host[:port]` (a value without a scheme means http) and
//!   `https://...`, which talks TLS to the proxy too; the user and password are sent as
//!   `Proxy-Authorization: Basic`;
//! - `socks5://[user:password@]host[:port]` (names resolved here) and `socks5h://...` (names
//!   resolved by the proxy); the default port is 1080.
//!
//! Any other scheme, and a value that is not a URL, is an error up front rather than a silently
//! ignored proxy. The metadata server of Google Cloud is always reached directly: it is a
//! link-local address that a proxy cannot reach for this machine.
//!
//! The proxy URL can hold a password, so it is never printed: errors name the variable it came
//! from, not its value.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use hyper::Uri;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder, MaybeHttpsStream};
use hyper_util::{
    client::{
        legacy::connect::{
            proxy::{SocksV5, Tunnel},
            HttpConnector,
        },
        proxy::matcher::{Intercept, Matcher},
    },
    rt::TokioIo,
};
use tokio::net::TcpStream;
use tower_service::Service;

#[cfg(test)]
pub(crate) mod test_servers;

type BoxError = Box<dyn Error + Send + Sync>;

/// The proxy variables in the order of hyper-util's `Matcher::from_env`: of each pair the first
/// one that is set counts.
const ALL_PROXY: [&str; 2] = ["ALL_PROXY", "all_proxy"];
const HTTPS_PROXY: [&str; 2] = ["HTTPS_PROXY", "https_proxy"];
const HTTP_PROXY: [&str; 2] = ["HTTP_PROXY", "http_proxy"];
const NO_PROXY: [&str; 2] = ["NO_PROXY", "no_proxy"];

/// Hosts that are never reached through a proxy: the metadata server of Google Cloud.
const METADATA_HOSTS: [&str; 3] = [
    "metadata.google.internal",
    "metadata.google.internal.",
    "169.254.169.254",
];

const DEFAULT_SOCKS_PORT: u16 = 1080;

/// A connector for hyper's client that connects directly or through the proxy the environment
/// names for the destination. It yields plain TCP streams to the destination (TLS to the
/// destination is up to the connector around it), or the stream of a TLS session with an
/// https:// proxy that carries the tunnel.
#[derive(Clone)]
pub(crate) struct ProxyConnector {
    http: HttpConnector,
    routes: Option<Arc<Routes>>,
}

struct Routes {
    matcher: Matcher,
    /// The variables the proxies for http:// and https:// destinations come from.
    http_var: Option<&'static str>,
    https_var: Option<&'static str>,
    /// For https:// proxies: TLS to the proxy, HTTP/1.1 only (CONNECT is an HTTP/1.1 request).
    proxy_tls: Option<HttpsConnector<HttpConnector>>,
    direct_hosts: Vec<String>,
}

impl ProxyConnector {
    /// Connects every destination directly.
    pub(crate) fn direct() -> Self {
        ProxyConnector {
            http: http_connector(),
            routes: None,
        }
    }

    /// Uses the proxies of the environment of this process.
    pub(crate) fn from_env() -> Result<Self, ProxyError> {
        Self::from_lookup(|name| std::env::var(name).ok())
    }

    /// Uses the proxies of the environment that `lookup` reads.
    pub(crate) fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ProxyError> {
        // Like curl and hyper-util: in a CGI script, HTTP_PROXY may come from a request header
        // ("httpoxy"), so no proxy variable is trusted there.
        if lookup("REQUEST_METHOD").is_some() {
            return Ok(Self::direct());
        }

        let setting = |names: [&'static str; 2]| -> Result<Option<Setting>, ProxyError> {
            let Some((var, value)) = names
                .iter()
                .find_map(|&name| lookup(name).map(|value| (name, value)))
            else {
                return Ok(None);
            };
            Setting::parse(var, &value)
        };
        let all = setting(ALL_PROXY)?;
        let https = setting(HTTPS_PROXY)?;
        let http = setting(HTTP_PROXY)?;

        let var_for_https = https.as_ref().or(all.as_ref()).map(|setting| setting.var);
        let var_for_http = http.as_ref().or(all.as_ref()).map(|setting| setting.var);
        if var_for_https.is_none() && var_for_http.is_none() {
            return Ok(Self::direct());
        }

        let mut builder = Matcher::builder();
        let mut tls_proxy = false;
        for setting in [&all, &https, &http].into_iter().flatten() {
            tls_proxy |= setting.scheme == "https";
        }
        if let Some(all) = all {
            builder = builder.all(all.value);
        }
        if let Some(https) = https {
            builder = builder.https(https.value);
        }
        if let Some(http) = http {
            builder = builder.http(http.value);
        }
        if let Some(no) = NO_PROXY.iter().find_map(|&name| lookup(name)) {
            builder = builder.no(no);
        }

        let proxy_tls = if tls_proxy {
            Some(
                HttpsConnectorBuilder::new()
                    .with_native_roots()
                    .map_err(ProxyError::Tls)?
                    .https_or_http()
                    .enable_http1()
                    .build(),
            )
        } else {
            None
        };

        let mut direct_hosts: Vec<String> = METADATA_HOSTS.map(str::to_owned).to_vec();
        if let Some(host) = lookup("GCE_METADATA_HOST") {
            if let Ok(authority) = host.trim().parse::<hyper::http::uri::Authority>() {
                direct_hosts.push(authority.host().to_owned());
            }
        }

        Ok(ProxyConnector {
            http: http_connector(),
            routes: Some(Arc::new(Routes {
                matcher: builder.build(),
                http_var: var_for_http,
                https_var: var_for_https,
                proxy_tls,
                direct_hosts,
            })),
        })
    }
}

fn http_connector() -> HttpConnector {
    let mut http = HttpConnector::new();
    // https:// destinations come here too: TLS is added by the connector around this one.
    http.enforce_http(false);
    http
}

/// One proxy variable, checked.
struct Setting {
    var: &'static str,
    /// The value, with its scheme in lowercase.
    value: String,
    scheme: String,
}

impl Setting {
    /// `Ok(None)` for an empty value, which hyper-util also ignores.
    fn parse(var: &'static str, value: &str) -> Result<Option<Setting>, ProxyError> {
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        // Lowercase the scheme: hyper-util compares the socks schemes case-sensitively.
        let value = match value.split_once("://") {
            Some((scheme, rest)) => format!("{}://{rest}", scheme.to_ascii_lowercase()),
            None => value.to_owned(),
        };

        let uri: Uri = value.parse().map_err(|_| ProxyError::Invalid { var })?;
        let scheme = uri.scheme_str().unwrap_or("http").to_owned();
        match scheme.as_str() {
            "http" | "https" | "socks5" | "socks5h" => {}
            _ => {
                return Err(ProxyError::UnsupportedScheme {
                    var,
                    scheme: printable_scheme(&scheme),
                })
            }
        }
        if uri.host().is_none_or(str::is_empty) {
            return Err(ProxyError::Invalid { var });
        }
        Ok(Some(Setting { var, value, scheme }))
    }
}

/// `scheme` if it looks like a scheme, so that it can be printed: it is part of the variable,
/// which is otherwise never printed.
fn printable_scheme(scheme: &str) -> Option<String> {
    let is_scheme = scheme.len() <= 16
        && scheme
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    is_scheme.then(|| scheme.to_owned())
}

impl Routes {
    /// The proxy for `dst` and the variable it comes from; `None` to connect directly.
    fn intercept(&self, dst: &Uri) -> Option<(&'static str, Intercept)> {
        let host = dst.host()?;
        if self
            .direct_hosts
            .iter()
            .any(|direct| direct.eq_ignore_ascii_case(host))
        {
            return None;
        }
        let var = match dst.scheme_str() {
            Some("https") => self.https_var?,
            Some("http") => self.http_var?,
            _ => return None,
        };
        self.matcher
            .intercept(dst)
            .map(|intercept| (var, intercept))
    }
}

type Stream = MaybeHttpsStream<TokioIo<TcpStream>>;
type Connecting = Pin<Box<dyn Future<Output = Result<Stream, BoxError>> + Send>>;

impl Service<Uri> for ProxyConnector {
    type Response = Stream;
    type Error = BoxError;
    type Future = Connecting;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.http.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        let Some((var, intercept)) = self
            .routes
            .as_ref()
            .and_then(|routes| routes.intercept(&dst))
        else {
            let connecting = self.http.call(dst);
            return Box::pin(async move { Ok(MaybeHttpsStream::Http(connecting.await?)) });
        };

        // hyper-util's connectors take port 443 for a destination without a port.
        let dst = with_default_port(dst);
        let proxy = intercept.uri().clone();
        let through =
            move |source: BoxError| -> BoxError { Box::new(ProxyConnectError { var, source }) };

        match proxy.scheme_str() {
            Some(scheme @ ("socks5" | "socks5h")) => {
                let proxy = with_port(&proxy, DEFAULT_SOCKS_PORT);
                let local_dns = scheme == "socks5";
                let auth = intercept
                    .raw_auth()
                    .map(|(user, password)| (user.to_owned(), password.to_owned()));
                // Connected here and not by SocksV5, whose error for a failed connection to the
                // proxy drops the reason.
                let connecting = self.http.call(proxy.clone());
                Box::pin(async move {
                    let stream = connecting.await.map_err(|error| through(Box::new(error)))?;
                    let mut socks =
                        SocksV5::new(proxy, Connected(Some(stream))).local_dns(local_dns);
                    if let Some((user, password)) = auth {
                        socks = socks.with_auth(user, password);
                    }
                    let stream = socks
                        .call(dst)
                        .await
                        .map_err(|error| through(Box::new(error)))?;
                    Ok(MaybeHttpsStream::Http(stream))
                })
            }
            Some("https") => {
                let Some(proxy_tls) = self
                    .routes
                    .as_ref()
                    .and_then(|routes| routes.proxy_tls.clone())
                else {
                    return Box::pin(async move {
                        Err(through(Box::new(ProxyError::Tls(io::Error::other(
                            "no TLS connector for the proxy",
                        )))))
                    });
                };
                let mut tunnel = Tunnel::new(proxy, proxy_tls);
                if let Some(auth) = intercept.basic_auth() {
                    tunnel = tunnel.with_auth(auth.clone());
                }
                let connecting = tunnel.call(dst);
                Box::pin(async move { connecting.await.map_err(|error| through(Box::new(error))) })
            }
            _ => {
                let mut tunnel = Tunnel::new(proxy, self.http.clone());
                if let Some(auth) = intercept.basic_auth() {
                    tunnel = tunnel.with_auth(auth.clone());
                }
                let connecting = tunnel.call(dst);
                Box::pin(async move {
                    let stream = connecting.await.map_err(|error| through(Box::new(error)))?;
                    Ok(MaybeHttpsStream::Http(stream))
                })
            }
        }
    }
}

/// A connector that hands out one connection that is open already.
struct Connected(Option<TokioIo<TcpStream>>);

impl Service<Uri> for Connected {
    type Response = TokioIo<TcpStream>;
    type Error = io::Error;
    type Future = std::future::Ready<io::Result<TokioIo<TcpStream>>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        std::future::ready(
            self.0
                .take()
                .ok_or_else(|| io::Error::other("the connection to the proxy was used already")),
        )
    }
}

/// `uri` with the default port of its scheme if it has none.
fn with_default_port(uri: Uri) -> Uri {
    if uri.port().is_some() {
        return uri;
    }
    let (Some(scheme), Some(host)) = (uri.scheme_str(), uri.host()) else {
        return uri;
    };
    let port = if scheme == "http" { 80 } else { 443 };
    let path = uri.path_and_query().map_or("/", |path| path.as_str());
    format!("{scheme}://{host}:{port}{path}")
        .parse()
        .unwrap_or(uri)
}

/// `proxy` with `port` if it has none.
fn with_port(proxy: &Uri, port: u16) -> Uri {
    if proxy.port().is_some() {
        return proxy.clone();
    }
    let host = proxy.host().unwrap_or_default();
    let scheme = proxy.scheme_str().unwrap_or("http");
    format!("{scheme}://{host}:{port}/")
        .parse()
        .unwrap_or_else(|_| proxy.clone())
}

/// A proxy variable that cannot be used. Names the variable, never its value.
#[derive(Debug)]
pub(crate) enum ProxyError {
    Invalid {
        var: &'static str,
    },
    UnsupportedScheme {
        var: &'static str,
        scheme: Option<String>,
    },
    Tls(io::Error),
}

impl Display for ProxyError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            ProxyError::Invalid { var } => write!(
                f,
                "the proxy in {var} is not a URL like http://host:port; set it to one or unset it"
            ),
            ProxyError::UnsupportedScheme { var, scheme } => {
                f.write_str("the proxy in ")?;
                f.write_str(var)?;
                match scheme.as_deref() {
                    Some(scheme) if scheme.starts_with("socks") => {
                        write!(f, " is a {scheme} proxy; socks proxies other than socks5 are not supported")?;
                    }
                    Some(scheme) => {
                        write!(f, " has the scheme '{scheme}', which is not supported")?;
                    }
                    None => f.write_str(" has a scheme that is not supported")?,
                }
                f.write_str("; use http://, https://, socks5:// or socks5h://")
            }
            ProxyError::Tls(_) => f.write_str("unable to set up TLS for the https proxy"),
        }
    }
}

impl Error for ProxyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ProxyError::Tls(source) => Some(source),
            ProxyError::Invalid { .. } | ProxyError::UnsupportedScheme { .. } => None,
        }
    }
}

/// A connection through a proxy failed.
#[derive(Debug)]
struct ProxyConnectError {
    var: &'static str,
    source: BoxError,
}

impl Display for ProxyConnectError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "unable to connect through the proxy set in {}", self.var)
    }
}

impl Error for ProxyConnectError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.source.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use serde_json::json;

    use super::{test_servers::TestProxy, *};
    use crate::{
        auth::{
            service::{parse_key, ServiceCredentials},
            test_support::{service_account_key_json, MockServer as TokenServer},
        },
        common::mock_server::{MockServer as DriveServer, Response},
        hub::{self, Hub},
    };

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    fn connector(vars: &[(&str, &str)]) -> ProxyConnector {
        ProxyConnector::from_lookup(lookup(vars)).unwrap()
    }

    /// The variable whose proxy `dst` goes through; `None` for a direct connection.
    fn route(vars: &[(&str, &str)], dst: &str) -> Option<&'static str> {
        let connector = connector(vars);
        let routes = connector.routes.as_ref()?;
        routes.intercept(&dst.parse().unwrap()).map(|(var, _)| var)
    }

    fn error(vars: &[(&str, &str)]) -> ProxyError {
        ProxyConnector::from_lookup(lookup(vars)).err().unwrap()
    }

    const DRIVE: &str = "https://www.googleapis.com/drive/v3/about";
    const PLAIN: &str = "http://example.com/";

    #[test]
    fn each_scheme_gets_its_proxy_and_all_proxy_fills_in() {
        let vars = [
            ("HTTPS_PROXY", "http://secure:3128"),
            ("HTTP_PROXY", "http://plain:3128"),
        ];
        assert_eq!(route(&vars, DRIVE), Some("HTTPS_PROXY"));
        assert_eq!(route(&vars, PLAIN), Some("HTTP_PROXY"));

        let vars = [
            ("ALL_PROXY", "socks5h://socks:1080"),
            ("HTTP_PROXY", "plain:3128"),
        ];
        assert_eq!(route(&vars, DRIVE), Some("ALL_PROXY"));
        assert_eq!(route(&vars, PLAIN), Some("HTTP_PROXY"));

        // Lowercase names work; the uppercase name wins when both are set.
        assert_eq!(
            route(&[("https_proxy", "proxy:3128")], DRIVE),
            Some("https_proxy")
        );
        let both = [("https_proxy", "lower:3128"), ("HTTPS_PROXY", "upper:3128")];
        assert_eq!(route(&both, DRIVE), Some("HTTPS_PROXY"));

        // Only HTTP_PROXY: https destinations stay direct, as with curl.
        assert_eq!(route(&[("HTTP_PROXY", "plain:3128")], DRIVE), None);
    }

    #[test]
    fn no_proxy_and_the_metadata_server_are_reached_directly() {
        let vars = [
            ("ALL_PROXY", "http://proxy:3128"),
            ("NO_PROXY", "localhost, .internal.example.com,10.0.0.0/8"),
        ];
        assert_eq!(route(&vars, DRIVE), Some("ALL_PROXY"));
        assert_eq!(route(&vars, "https://files.internal.example.com/"), None);
        assert_eq!(route(&vars, "http://10.1.2.3:8080/"), None);
        assert_eq!(route(&vars, "http://localhost:8080/"), None);
        assert_eq!(
            route(&[("no_proxy", "*"), ("ALL_PROXY", "proxy:1")], DRIVE),
            None
        );

        for metadata in [
            "http://metadata.google.internal/computeMetadata/v1/",
            "http://169.254.169.254/computeMetadata/v1/",
        ] {
            assert_eq!(route(&vars, metadata), None, "{metadata}");
        }
        let custom = [
            ("ALL_PROXY", "proxy:3128"),
            ("GCE_METADATA_HOST", "10.9.8.7:80"),
        ];
        assert_eq!(
            route(&custom, "http://10.9.8.7:80/computeMetadata/v1/"),
            None
        );
    }

    #[test]
    fn without_proxy_variables_everything_is_direct() {
        assert!(connector(&[]).routes.is_none());
        assert!(connector(&[("HTTPS_PROXY", ""), ("ALL_PROXY", "  ")])
            .routes
            .is_none());
        // An empty uppercase variable hides the lowercase one, as in hyper-util.
        assert!(
            connector(&[("HTTPS_PROXY", ""), ("https_proxy", "proxy:1")])
                .routes
                .is_none()
        );
        // CGI: HTTP_PROXY may come from a request header.
        let cgi = [("REQUEST_METHOD", "GET"), ("HTTP_PROXY", "http://evil:80")];
        assert!(connector(&cgi).routes.is_none());
    }

    #[test]
    fn bad_proxy_values_are_errors_that_never_show_the_value() {
        let cases = [
            (
                "ALL_PROXY",
                "socks4://user:secret@proxy:1080",
                "is a socks4 proxy; socks proxies other than socks5 are not supported",
            ),
            (
                "ALL_PROXY",
                "SOCKS4A://user:secret@proxy:1080",
                "is a socks4a proxy",
            ),
            (
                "HTTPS_PROXY",
                "ftp://user:secret@proxy:21",
                "has the scheme 'ftp', which is not supported",
            ),
            (
                "https_proxy",
                "http://user:sec ret@proxy:3128",
                "is not a URL like http://host:port",
            ),
            ("HTTP_PROXY", "http://", "is not a URL"),
            ("HTTP_PROXY", "secret\u{1}garbage://x", "is not a URL"),
        ];
        for (var, value, expected) in cases {
            let error = error(&[(var, value)]);
            let message = error.to_string();
            assert!(
                message.starts_with(&format!("the proxy in {var} ")),
                "{value}: {message}"
            );
            assert!(message.contains(expected), "{value}: {message}");
            assert!(!message.contains("secret"), "{value}: {message}");
            assert!(!message.contains("proxy:"), "{value}: {message}");
            assert!(
                !format!("{error:?}").contains("secret"),
                "{value}: {error:?}"
            );
        }

        // A scheme that is not printable is not repeated either.
        let error = error(&[("ALL_PROXY", "pass+word+that+is+long://proxy:1")]);
        assert!(!error.to_string().contains("pass"), "{error}");

        // Through the client constructors, as io::Error.
        let error = io::Error::other(super::ProxyError::UnsupportedScheme {
            var: "ALL_PROXY",
            scheme: Some(String::from("socks4")),
        });
        assert!(error.to_string().contains("socks4"), "{error}");
    }

    #[test]
    fn supported_values_are_accepted() {
        for value in [
            "proxy:3128",
            "http://proxy",
            "HTTP://user:p%40ss@proxy:3128/",
            "https://proxy.example.com:443",
            "socks5://127.0.0.1",
            "Socks5h://user:pass@[::1]:1080",
        ] {
            assert!(
                ProxyConnector::from_lookup(lookup(&[("ALL_PROXY", value)])).is_ok(),
                "{value}"
            );
        }
    }

    #[test]
    fn destinations_and_socks_proxies_get_their_default_ports() {
        let with = |uri: &str| with_default_port(uri.parse().unwrap()).to_string();
        assert_eq!(with("http://metadata/x?y=1"), "http://metadata:80/x?y=1");
        assert_eq!(
            with("https://www.googleapis.com/"),
            "https://www.googleapis.com:443/"
        );
        assert_eq!(with("http://127.0.0.1:8080/"), "http://127.0.0.1:8080/");

        let socks = |uri: &str| with_port(&uri.parse().unwrap(), DEFAULT_SOCKS_PORT).to_string();
        assert_eq!(socks("socks5://proxy/"), "socks5://proxy:1080/");
        assert_eq!(socks("socks5h://proxy:9050/"), "socks5h://proxy:9050/");
    }

    async fn about(hub: &Hub) -> Result<Option<String>, Box<google_drive3::Error>> {
        let (_, about) = hub
            .about()
            .get()
            .param("fields", "user")
            .doit()
            .await
            .map_err(Box::new)?;
        Ok(about.user.and_then(|user| user.email_address))
    }

    fn me() -> Response {
        Response::json(200, &json!({"user": {"emailAddress": "me@example.com"}}))
    }

    #[tokio::test]
    async fn drive_requests_go_through_a_connect_tunnel_with_basic_auth() {
        let drive = DriveServer::start(vec![me()]).await;
        let root_url = format!("http://{}/", drive.addr());
        // "user:p@ss", the @ percent-encoded in the URL.
        let expected_auth = "Basic dXNlcjpwQHNz";
        let proxy = TestProxy::connect(Some(expected_auth)).await;
        let value = format!("http://user:p%40ss@{}", proxy.addr());

        let hub =
            Hub::for_mock_server_through(&root_url, connector(&[("HTTP_PROXY", &value)])).unwrap();
        assert_eq!(
            about(&hub).await.unwrap().as_deref(),
            Some("me@example.com")
        );

        let requests = proxy.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].target, drive.addr().to_string());
        assert_eq!(requests[0].auth.as_deref(), Some(expected_auth));
        assert_eq!(drive.requests()[0].path, "/drive/v3/about");
    }

    #[tokio::test]
    async fn no_proxy_bypasses_the_proxy() {
        let drive = DriveServer::start(vec![me()]).await;
        let root_url = format!("http://{}/", drive.addr());
        let proxy = TestProxy::connect(None).await;
        let value = format!("http://{}", proxy.addr());

        let vars = [("ALL_PROXY", value.as_str()), ("NO_PROXY", "127.0.0.1")];
        let hub = Hub::for_mock_server_through(&root_url, connector(&vars)).unwrap();
        assert_eq!(
            about(&hub).await.unwrap().as_deref(),
            Some("me@example.com")
        );

        assert_eq!(proxy.requests(), Vec::new());
        assert_eq!(drive.requests().len(), 1);
    }

    #[tokio::test]
    async fn service_account_tokens_go_through_the_proxy() {
        let token_server = TokenServer::start(vec![(
            200,
            String::from(
                r#"{"access_token":"ya29.proxied","expires_in":3599,"token_type":"Bearer"}"#,
            ),
        )])
        .await;
        let proxy = TestProxy::connect(None).await;
        let value = format!("http://{}", proxy.addr());
        let client = hub::token_http_client_with(connector(&[("http_proxy", &value)])).unwrap();
        let key =
            parse_key(service_account_key_json(&token_server.url("/token")).as_bytes()).unwrap();

        let credentials = ServiceCredentials::service_account(client, key, None, None)
            .await
            .unwrap();
        assert_eq!(
            credentials.access_token().await.unwrap().token(),
            Some("ya29.proxied")
        );

        assert_eq!(proxy.requests()[0].target, token_server.addr().to_string());
        assert_eq!(token_server.requests().len(), 1);
    }

    #[tokio::test]
    async fn token_refreshes_tunnel_to_google_through_the_proxy() {
        use crate::{app_config::Secret, auth::stored::StoredCredentials};

        let proxy = TestProxy::connect(None).await;
        let value = format!("http://user:hunter2@{}", proxy.addr());
        let client = hub::token_http_client_with(connector(&[("HTTPS_PROXY", &value)])).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let tokens = dir.path().join("tokens.json");
        std::fs::write(
            &tokens,
            r#"[{"scopes":["https://www.googleapis.com/auth/drive","https://www.googleapis.com/auth/drive.metadata.readonly"],"token":{"access_token":"old","refresh_token":"1//r","expires_at":[2020,1,0,0,0,0,0,0,0],"id_token":null}}]"#,
        )
        .unwrap();
        let secret = Secret {
            client_id: String::from("id"),
            client_secret: String::from("secret"),
        };

        let credentials = StoredCredentials::load(client, "me@example.com", &secret, &tokens)
            .await
            .unwrap();
        // The test proxy refuses every target off the loopback interface with a 403.
        let error = credentials
            .access_token(&crate::auth::DRIVE_SCOPES)
            .await
            .unwrap_err();

        assert_eq!(proxy.requests()[0].target, "accounts.google.com:443");
        let message = crate::error_message(&error);
        assert!(
            message.contains("unable to connect through the proxy set in HTTPS_PROXY: tunnel error: unsuccessful"),
            "{message}"
        );
        assert!(!message.contains("hunter2"), "{message}");
        assert!(!message.contains(&proxy.addr().to_string()), "{message}");
    }

    #[tokio::test]
    async fn a_refused_proxy_login_is_reported() {
        let drive = DriveServer::start(vec![me()]).await;
        let proxy = TestProxy::connect(Some("Basic other")).await;
        let value = format!("http://user:hunter2@{}", proxy.addr());
        let hub = Hub::for_mock_server_through(
            &format!("http://{}/", drive.addr()),
            connector(&[("ALL_PROXY", &value)]),
        )
        .unwrap();

        let error = about(&hub).await.unwrap_err();
        let message = crate::error_message(&*error);
        assert!(
            message.contains("unable to connect through the proxy set in ALL_PROXY: tunnel error: proxy authorization required"),
            "{message}"
        );
        assert!(!message.contains("hunter2"), "{message}");
        assert!(drive.requests().is_empty());
    }

    #[tokio::test]
    async fn socks5_proxies_with_and_without_remote_dns() {
        let drive = DriveServer::start(vec![me(), me()]).await;
        let port = drive.addr().port();
        let proxy = TestProxy::socks5().await;

        // socks5: the address is resolved here and sent as an IP address.
        let value = format!("socks5://robot:pa55@{}", proxy.addr());
        let hub = Hub::for_mock_server_through(
            &format!("http://127.0.0.1:{port}/"),
            connector(&[("ALL_PROXY", &value)]),
        )
        .unwrap();
        assert_eq!(
            about(&hub).await.unwrap().as_deref(),
            Some("me@example.com")
        );

        // socks5h: the proxy gets the host name.
        let value = format!("socks5h://{}", proxy.addr());
        let hub = Hub::for_mock_server_through(
            &format!("http://localhost:{port}/"),
            connector(&[("ALL_PROXY", &value)]),
        )
        .unwrap();
        assert_eq!(
            about(&hub).await.unwrap().as_deref(),
            Some("me@example.com")
        );

        let requests = proxy.requests();
        assert_eq!(requests[0].target, format!("127.0.0.1:{port}"));
        assert_eq!(requests[0].auth.as_deref(), Some("robot:pa55"));
        assert_eq!(requests[1].target, format!("localhost:{port}"));
        assert_eq!(requests[1].auth, None);
        assert_eq!(drive.requests().len(), 2);
    }
}
