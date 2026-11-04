use std::{io, ops::Deref};

use google_apis_common::GetToken;
use google_drive3::DriveHub;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::Client;

use crate::proxy::ProxyConnector;

/// The connector of every HTTP client: rustls over a direct or proxied connection.
pub(crate) type Connector = HttpsConnector<ProxyConnector>;

/// Cloning is cheap and shares the connection pool and the credentials (and so the token cache).
#[derive(Clone)]
pub(crate) struct Hub(DriveHub<Connector>);

impl Deref for Hub {
    type Target = DriveHub<Connector>;

    fn deref(&self) -> &Self::Target {
        &self.0
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

        Ok(Hub(google_drive3::DriveHub::new(http_client, auth)))
    }
}

impl Hub {
    /// The Drive API base URL (`https://www.googleapis.com/drive/v3/` unless a test changed it),
    /// for requests google-drive3 cannot build, such as ones with extra headers.
    pub(crate) fn drive_base_url(&self) -> String {
        // google-drive3 only has a setter, which returns the previous value; the clone shares
        // the client and credentials and is dropped right away.
        self.0.clone().base_url(String::new())
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
        hub.base_url(base_url);
        Ok(Hub(hub))
    }
}

#[cfg(test)]
impl Hub {
    /// Hub that sends every request to the server at `root_url` (for example
    /// `http://127.0.0.1:1234/`) with a dummy bearer token. Used with `common::mock_server`.
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
        hub.root_url(root_url.to_owned());
        hub.base_url(format!("{root_url}drive/v3/"));
        Ok(Hub(hub))
    }
}
