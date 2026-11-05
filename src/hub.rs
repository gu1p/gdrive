use std::{io, ops::Deref};

use google_apis_common::GetToken;
use google_drive3::DriveHub;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;

use crate::{
    common::retry::{Retrier, RetryPolicy, Throttle},
    proxy::ProxyConnector,
};

/// The connector of every HTTP client: rustls over a direct or proxied connection.
pub(crate) type Connector = HttpsConnector<ProxyConnector>;

/// The Drive API client of a command: google-drive3's hub, and the retry policy and throttle
/// that every request made through it shares.
///
/// Cloning is cheap and shares the connection pool, the credentials (and so the token cache) and
/// the throttle, so that a rate limit seen by one clone holds back the others.
#[derive(Clone)]
pub(crate) struct Hub {
    drive: DriveHub<Connector>,
    /// The Drive API base URL the hub was set up with (google-drive3 has no getter for it).
    drive_base_url: String,
    /// The base URL of the Admin SDK Reports API, which google-drive3 does not know.
    reports_base_url: String,
    retrier: Retrier,
}

/// google-drive3's default base URL.
const DRIVE_BASE_URL: &str = "https://www.googleapis.com/drive/v3/";

/// The base URL of the Admin SDK Reports API (the audit log of a Google Workspace organization).
const REPORTS_BASE_URL: &str = "https://admin.googleapis.com/admin/reports/v1/";

/// Where the Reports API is below a server's root URL, as for Google's own.
#[cfg(test)]
const REPORTS_PATH: &str = "admin/reports/v1/";

impl Deref for Hub {
    type Target = DriveHub<Connector>;

    fn deref(&self) -> &Self::Target {
        &self.drive
    }
}

/// The connector for the proxies of the environment (see [`crate::proxy`]).
fn create_connector() -> io::Result<Connector> {
    let proxy = ProxyConnector::from_env().map_err(io::Error::other)?;
    connector_with(proxy)
}

/// TLS is added over the proxy connection, so it runs end to end to Google.
fn connector_with(proxy: ProxyConnector) -> io::Result<Connector> {
    let connector = HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .wrap_connector(proxy);
    Ok(connector)
}

impl Hub {
    pub(crate) fn new(auth: impl GetToken + 'static) -> io::Result<Hub> {
        let http_client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(create_connector()?);

        Ok(Hub::from_drive_hub(
            google_drive3::DriveHub::new(http_client, auth),
            DRIVE_BASE_URL.to_owned(),
            REPORTS_BASE_URL.to_owned(),
            RetryPolicy::default(),
        ))
    }

    fn from_drive_hub(
        drive: DriveHub<Connector>,
        drive_base_url: String,
        reports_base_url: String,
        policy: RetryPolicy,
    ) -> Hub {
        Hub {
            drive,
            drive_base_url,
            reports_base_url,
            retrier: Retrier::new(policy, Throttle::new()),
        }
    }

    /// The Drive API base URL, ending in `/` (`https://www.googleapis.com/drive/v3/` unless a
    /// test changed it), for requests google-drive3 cannot build, such as ones with extra
    /// headers. google-drive3 only has a setter for it.
    pub(crate) fn drive_base_url(&self) -> &str {
        &self.drive_base_url
    }

    /// The Admin SDK Reports API base URL, ending in `/`
    /// (`https://admin.googleapis.com/admin/reports/v1/` unless a test changed it).
    pub(crate) fn reports_base_url(&self) -> &str {
        &self.reports_base_url
    }

    /// The retry policy and the throttle shared by every request of this hub.
    pub(crate) fn retrier(&self) -> &Retrier {
        &self.retrier
    }
}

/// The HTTP client for requests to the OAuth token endpoint.
pub(crate) fn token_http_client() -> io::Result<Client<Connector, String>> {
    Ok(Client::builder(hyper_util::rt::TokioExecutor::new()).build(create_connector()?))
}

#[cfg(test)]
/// [`token_http_client`] with the proxy `proxy`.
pub(crate) fn token_http_client_with(
    proxy: ProxyConnector,
) -> io::Result<Client<Connector, String>> {
    Ok(Client::builder(hyper_util::rt::TokioExecutor::new()).build(connector_with(proxy)?))
}

#[cfg(test)]
impl Hub {
    /// A hub without credentials that sends every Drive request to `base_url`, for tests
    /// against a local stub server.
    pub(crate) fn with_base_url(base_url: String) -> io::Result<Hub> {
        let http_client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(connector_with(ProxyConnector::direct())?);
        let mut hub = google_drive3::DriveHub::new(http_client, google_apis_common::NoToken);
        hub.base_url(base_url.clone());
        let root = base_url.strip_suffix("drive/v3/").unwrap_or(&base_url);
        let reports_base_url = format!("{root}{REPORTS_PATH}");
        Ok(Hub::from_drive_hub(
            hub,
            base_url,
            reports_base_url,
            RetryPolicy::for_tests(),
        ))
    }
}

#[cfg(test)]
impl Hub {
    /// Hub that sends every request to the server at `root_url` (for example
    /// `http://127.0.0.1:1234/`) with a dummy bearer token, for any scope. Used with
    /// `common::mock_server`. Requests to the Reports API go to `<root_url>admin/reports/v1/`.
    pub(crate) fn for_mock_server(root_url: &str) -> io::Result<Hub> {
        Self::for_mock_server_through(root_url, ProxyConnector::direct())
    }

    /// [`Hub::for_mock_server`] with the proxy `proxy`.
    pub(crate) fn for_mock_server_through(
        root_url: &str,
        proxy: ProxyConnector,
    ) -> io::Result<Hub> {
        let http_client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(connector_with(proxy)?);

        // Not a credential: google-drive3 refuses resumable uploads without some token.
        let mut hub = DriveHub::new(http_client, String::from("mock-token"));
        let base_url = format!("{root_url}drive/v3/");
        hub.root_url(root_url.to_owned());
        hub.base_url(base_url.clone());
        Ok(Hub::from_drive_hub(
            hub,
            base_url,
            format!("{root_url}{REPORTS_PATH}"),
            RetryPolicy::for_tests(),
        ))
    }
}
