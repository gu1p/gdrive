use std::{io, ops::Deref};

use google_apis_common::GetToken;
use google_drive3::DriveHub;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{connect::HttpConnector, Client};

use crate::common::retry::{Retrier, RetryPolicy, Throttle};

/// The Drive API client of a command: google-drive3's hub, and the retry policy and throttle
/// that every request made through it shares.
///
/// Cloning is cheap and shares the connection pool, the credentials (and so the token cache) and
/// the throttle, so that a rate limit seen by one clone holds back the others.
#[derive(Clone)]
pub(crate) struct Hub {
    drive: DriveHub<HttpsConnector<HttpConnector>>,
    /// The Drive API base URL the hub was set up with (google-drive3 has no getter for it).
    drive_base_url: String,
    retrier: Retrier,
}

/// google-drive3's default base URL.
const DRIVE_BASE_URL: &str = "https://www.googleapis.com/drive/v3/";

impl Deref for Hub {
    type Target = DriveHub<HttpsConnector<HttpConnector>>;

    fn deref(&self) -> &Self::Target {
        &self.drive
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

        Ok(Hub::from_drive_hub(
            google_drive3::DriveHub::new(http_client, auth),
            DRIVE_BASE_URL.to_owned(),
            RetryPolicy::default(),
        ))
    }

    fn from_drive_hub(
        drive: DriveHub<HttpsConnector<HttpConnector>>,
        drive_base_url: String,
        policy: RetryPolicy,
    ) -> Hub {
        Hub {
            drive,
            drive_base_url,
            retrier: Retrier::new(policy, Throttle::new()),
        }
    }

    /// The Drive API base URL, ending in `/` (`https://www.googleapis.com/drive/v3/` unless a
    /// test changed it), for requests google-drive3 cannot build, such as ones with extra
    /// headers. google-drive3 only has a setter for it.
    pub(crate) fn drive_base_url(&self) -> &str {
        &self.drive_base_url
    }

    /// The retry policy and the throttle shared by every request of this hub.
    pub(crate) fn retrier(&self) -> &Retrier {
        &self.retrier
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
        hub.base_url(base_url.clone());
        Ok(Hub::from_drive_hub(hub, base_url, RetryPolicy::for_tests()))
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
        let base_url = format!("{root_url}drive/v3/");
        hub.root_url(root_url.to_owned());
        hub.base_url(base_url.clone());
        Ok(Hub::from_drive_hub(hub, base_url, RetryPolicy::for_tests()))
    }
}
