use std::{future::Future, io, ops::Deref, path::Path, pin::Pin};

use google_drive3::DriveHub;
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::connect::HttpConnector;
use yup_oauth2::{
    self, authenticator::Authenticator, authenticator_delegate::InstalledFlowDelegate,
};

use crate::app_config;

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
    pub(crate) fn new(auth: Auth) -> io::Result<Hub> {
        let http_client =
            hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                .build(create_connector()?);

        Ok(Hub(google_drive3::DriveHub::new(http_client, auth.0)))
    }
}

pub(crate) struct Auth(pub(crate) Authenticator<HttpsConnector<HttpConnector>>);

impl Deref for Auth {
    type Target = Authenticator<HttpsConnector<HttpConnector>>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Auth {
    pub(crate) async fn new(
        config: &app_config::Secret,
        tokens_path: &Path,
    ) -> Result<Auth, io::Error> {
        let secret = oauth2_secret(config);
        let delegate = Box::new(AuthDelegate);

        let auth = yup_oauth2::InstalledFlowAuthenticator::with_client(
            secret,
            yup_oauth2::InstalledFlowReturnMethod::HTTPPortRedirect(8085),
            yup_oauth2::client::CustomHyperClientBuilder::from(
                hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
                    .build(create_connector()?),
            ),
        )
        .persist_tokens_to_disk(tokens_path)
        .flow_delegate(delegate)
        .build()
        .await
        .map_err(|err| without_token_content(err, tokens_path))?;

        Ok(Auth(auth))
    }
}

/// yup-oauth2 reports a tokens file it cannot parse with the `serde_json` error, which quotes the
/// input it rejects ("invalid type: string \"ya29...\""), so it can contain a token. Keep only
/// the position.
fn without_token_content(err: io::Error, tokens_path: &Path) -> io::Error {
    let Some(json_error) = err
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<serde_json::Error>())
    else {
        return err;
    };

    io::Error::new(
        err.kind(),
        format!(
            "unable to parse the tokens in '{}' at line {} column {}",
            tokens_path.display(),
            json_error.line(),
            json_error.column()
        ),
    )
}

fn oauth2_secret(config: &app_config::Secret) -> yup_oauth2::ApplicationSecret {
    yup_oauth2::ApplicationSecret {
        client_id: config.client_id.clone(),
        client_secret: config.client_secret.clone(),
        token_uri: String::from("https://oauth2.googleapis.com/token"),
        auth_uri: String::from("https://accounts.google.com/o/oauth2/auth"),
        redirect_uris: vec![String::from("urn:ietf:wg:oauth:2.0:oob")],
        project_id: None,
        client_email: None,
        auth_provider_x509_cert_url: Some(String::from(
            "https://www.googleapis.com/oauth2/v1/certs",
        )),
        client_x509_cert_url: None,
    }
}

struct AuthDelegate;

impl InstalledFlowDelegate for AuthDelegate {
    fn present_user_url<'a>(
        &'a self,
        url: &'a str,
        _need_code: bool,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + 'a>> {
        Box::pin(async move {
            println!();
            println!();
            println!("Gdrive requires permissions to manage your files on Google Drive.");
            println!("Open the url in your browser and follow the instructions:");
            println!("{url}");
            Ok(String::new())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_parse_errors_do_not_print_the_token() {
        let content =
            r#"[{"scopes": ["https://www.googleapis.com/auth/drive"], "token": "ya29.secret"}]"#;
        // How yup-oauth2 12 reports it (storage.rs, JSONTokens::load_from_file)
        let json_error =
            serde_json::from_str::<Vec<std::collections::HashMap<String, Vec<String>>>>(content)
                .unwrap_err();
        assert!(
            json_error.to_string().contains("ya29.secret"),
            "{json_error}"
        );
        let err = io::Error::new(io::ErrorKind::InvalidData, json_error);

        let err = without_token_content(err, Path::new("/config/account/tokens.json"));

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            err.to_string(),
            "unable to parse the tokens in '/config/account/tokens.json' at line 1 column 77"
        );
        assert!(std::error::Error::source(&err).is_none());
    }

    #[test]
    fn other_errors_are_kept() {
        let err = io::Error::new(io::ErrorKind::NotFound, "no such file");

        let err = without_token_content(err, Path::new("tokens.json"));

        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert_eq!(err.to_string(), "no such file");
    }
}
