use std::{io, ops::Deref};

use google_apis_common::GetToken;
use google_drive3::DriveHub;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{connect::HttpConnector, Client};

pub(crate) struct Hub(DriveHub<HttpsConnector<HttpConnector>>);

impl Deref for Hub {
    type Target = DriveHub<HttpsConnector<HttpConnector>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn create_connector() -> io::Result<HttpsConnector<HttpConnector>> {
    let connector = HttpsConnectorBuilder::new()
        .with_native_roots()?
        .https_or_http()
        .enable_http1()
        .enable_http2()
        .build();
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

/// The HTTP client for requests to the OAuth token endpoint.
pub(crate) fn token_http_client() -> io::Result<Client<HttpsConnector<HttpConnector>, String>> {
    Ok(Client::builder(hyper_util::rt::TokioExecutor::new()).build(create_connector()?))
}

#[cfg(test)]
impl Hub {
    /// A hub without credentials that sends every Drive request to `base_url`, for tests
    /// against a local stub server.
    pub(crate) fn with_base_url(base_url: String) -> io::Result<Hub> {
        let http_client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(create_connector()?);
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
        let http_client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(create_connector()?);

        // Not a credential: google-drive3 refuses resumable uploads without some token.
        let mut hub = DriveHub::new(http_client, String::from("mock-token"));
        hub.root_url(root_url.to_owned());
        hub.base_url(format!("{root_url}drive/v3/"));
        Ok(Hub(hub))
    }
}
