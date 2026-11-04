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
