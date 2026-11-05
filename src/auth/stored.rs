//! Refresh-only credentials for every command except the login commands.
//!
//! The authenticator is yup-oauth2's `AuthorizedUserAuthenticator`. Its flow
//! (`yup_oauth2::authorized_user::AuthorizedUserFlow`, yup-oauth2 12.1) only POSTs a
//! `grant_type=refresh_token` request; it has no flow delegate and opens no listener. Because it
//! reports no application secret, `Authenticator::find_token_info` never takes the branch that
//! falls back to an interactive flow after a failed refresh: a cached, unexpired token is
//! returned as is, anything else is a refresh request whose error is returned to the caller.
//! So a command run from cron can never end up waiting for a browser on port 8085 (#33, #147,
//! #149).
//!
//! The token storage is `tokens.json` through [`TokenStore`], the same file and layout the
//! login commands write.

use std::{
    error,
    fmt::{self, Display, Formatter},
    future::Future,
    io,
    path::Path,
    pin::Pin,
    sync::Arc,
    time::Duration,
};

use hyper_util::client::legacy::{connect::Connect, Client};
use yup_oauth2::{
    authenticator::Authenticator, authorized_user::AuthorizedUserSecret, error::AuthErrorCode,
    AccessToken, AuthorizedUserAuthenticator, CustomHyperClientBuilder,
};

use super::{
    store::{self, TokenFile, TokenStore},
    ReauthReason, ReauthRequired,
};
use crate::app_config;

/// How long one token request (a refresh) may take before gdrive gives up.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(60);

/// The stored credentials of one account. Implements the Drive API's `GetToken`.
#[derive(Clone)]
pub(crate) struct StoredCredentials<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    source: Source<C>,
    account: Arc<str>,
    /// [`TokenFile::audit_log_granted`] of the tokens loaded.
    audit_log_granted: bool,
}

#[derive(Clone)]
enum Source<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    /// Uses the cached token and refreshes it with the stored refresh token when it expired.
    Refreshing(Authenticator<C>),
    /// No refresh token is stored (Google left it out, #150): the cached access token works
    /// until it expires, as in gdrive 3.9; only then is a new authorization needed.
    CachedOnly(Arc<TokenFile>),
}

impl<C> StoredCredentials<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    /// Loads the token of `account` from `tokens_path`. Fails with [`ReauthRequired`] when the
    /// file holds no token; no request is sent in that case. Without a refresh token the
    /// credentials only serve the cached access token (see [`StoredCredentials::access_token`]).
    pub(crate) async fn load(
        client: Client<C, String>,
        account: &str,
        secret: &app_config::Secret,
        tokens_path: &Path,
    ) -> Result<Self, Error> {
        let reauth = |reason| {
            Error::Reauth(ReauthRequired {
                account: account.to_owned(),
                reason,
            })
        };

        let file = TokenFile::load(tokens_path)
            .await
            .map_err(Error::LoadTokens)?
            .filter(|file| !file.is_empty())
            .ok_or_else(|| reauth(ReauthReason::NoStoredToken))?;
        let audit_log_granted = file.audit_log_granted();
        let Some(refresh_token) = file.refresh_token().map(str::to_owned) else {
            return Ok(StoredCredentials {
                source: Source::CachedOnly(Arc::new(file)),
                account: account.into(),
                audit_log_granted,
            });
        };

        let user_secret = AuthorizedUserSecret {
            client_id: secret.client_id.clone(),
            client_secret: secret.client_secret.clone(),
            refresh_token: refresh_token.clone(),
            key_type: String::from("authorized_user"),
        };
        let store = TokenStore::new(tokens_path.to_path_buf(), file, Some(refresh_token));
        let auth = AuthorizedUserAuthenticator::with_client(
            user_secret,
            CustomHyperClientBuilder::from(client),
        )
        .with_storage(Box::new(store))
        .build()
        .await
        .map_err(Error::Build)?;

        Ok(StoredCredentials {
            source: Source::Refreshing(auth),
            account: account.into(),
            audit_log_granted,
        })
    }

    /// Whether the stored tokens record that the account was granted read access to the audit
    /// log ([`TokenFile::audit_log_granted`]). Ask for a token for [`super::AUDIT_LOG_SCOPE`]
    /// only when they do.
    pub(crate) fn audit_log_granted(&self) -> bool {
        self.audit_log_granted
    }

    /// An access token for `scopes`: the stored one while it is valid, otherwise a refreshed
    /// one, which is saved to `tokens.json`. Without a stored refresh token, an expired token
    /// is a [`ReauthReason::NoRefreshToken`] error.
    pub(crate) async fn access_token(&self, scopes: &[&str]) -> Result<AccessToken, Error> {
        let auth = match &self.source {
            Source::Refreshing(auth) => auth,
            Source::CachedOnly(file) => {
                return match file.get(scopes) {
                    Some(token) if !token.is_expired() => Ok(AccessToken::from(token.clone())),
                    _ => Err(Error::Reauth(ReauthRequired {
                        account: self.account.to_string(),
                        reason: ReauthReason::NoRefreshToken,
                    })),
                };
            }
        };

        let result = tokio::time::timeout(REFRESH_TIMEOUT, auth.token(scopes))
            .await
            .map_err(|_| Error::Timeout)?;

        match result {
            Ok(token) => Ok(token),
            Err(yup_oauth2::Error::AuthError(error))
                if error.error == AuthErrorCode::InvalidGrant =>
            {
                Err(Error::Reauth(ReauthRequired {
                    account: self.account.to_string(),
                    reason: ReauthReason::RefreshTokenRejected(error.error_description),
                }))
            }
            Err(error) => Err(Error::Refresh(error)),
        }
    }
}

pub(super) type GetTokenOutput<'a> = Pin<
    Box<
        dyn Future<Output = Result<Option<String>, Box<dyn error::Error + Send + Sync>>>
            + Send
            + 'a,
    >,
>;

impl<C> google_apis_common::GetToken for StoredCredentials<C>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    fn get_token<'a>(&'a self, scopes: &'a [&str]) -> GetTokenOutput<'a> {
        Box::pin(async move {
            match self.access_token(scopes).await {
                Ok(token) => Ok(token.token().map(str::to_owned)),
                // Box the ReauthRequired itself: the Drive client hides the source of this error.
                Err(Error::Reauth(reauth)) => Err(reauth.into()),
                Err(error) => Err(error.into()),
            }
        })
    }
}

#[derive(Debug)]
pub(crate) enum Error {
    Reauth(ReauthRequired),
    LoadTokens(store::LoadError),
    Build(io::Error),
    Refresh(yup_oauth2::Error),
    Timeout,
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Error::Reauth(reauth) => reauth.fmt(f),
            Error::LoadTokens(_) => f.write_str("unable to load the stored tokens"),
            Error::Build(_) => f.write_str("unable to set up the authenticator"),
            Error::Refresh(_) => f.write_str("unable to refresh the access token"),
            Error::Timeout => write!(
                f,
                "no answer from the Google token endpoint within {} seconds",
                REFRESH_TIMEOUT.as_secs()
            ),
        }
    }
}

impl error::Error for Error {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            Error::Reauth(reauth) => Some(reauth),
            Error::LoadTokens(source) => Some(source),
            Error::Build(source) => Some(source),
            Error::Refresh(source) => Some(source),
            Error::Timeout => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{net::SocketAddr, path::PathBuf};

    use time::{Duration as TimeDuration, OffsetDateTime};
    use yup_oauth2::storage::TokenInfo;

    use super::*;
    use crate::auth::{
        store::TokenEntry,
        test_support::{redirected_client, MockServer, RedirectConnector},
        AUDIT_LOG_SCOPE, DRIVE_SCOPES,
    };

    /// The scopes a login with `--audit-log` stores its token for.
    const DRIVE_AND_AUDIT: [&str; 3] = [DRIVE_SCOPES[0], DRIVE_SCOPES[1], AUDIT_LOG_SCOPE];

    fn secret() -> app_config::Secret {
        app_config::Secret {
            client_id: String::from("client-id.apps.googleusercontent.com"),
            client_secret: String::from("client-secret"),
        }
    }

    fn token(access: &str, refresh: Option<&str>, expires_in: TimeDuration) -> TokenInfo {
        TokenInfo {
            access_token: Some(access.to_owned()),
            refresh_token: refresh.map(str::to_owned),
            expires_at: Some(OffsetDateTime::now_utc() + expires_in),
            id_token: None,
        }
    }

    async fn write_tokens(dir: &Path, token: TokenInfo) -> PathBuf {
        write_tokens_for(dir, &DRIVE_SCOPES, token).await
    }

    /// A tokens.json with one entry, `token` for `scopes`.
    async fn write_tokens_for(dir: &Path, scopes: &[&str], token: TokenInfo) -> PathBuf {
        let path = dir.join("tokens.json");
        let json = serde_json::to_string(&[TokenEntry {
            scopes: scopes.iter().map(|&scope| scope.to_owned()).collect(),
            token,
            credential: None,
        }])
        .unwrap();
        tokio::fs::write(&path, json).await.unwrap();
        path
    }

    async fn load(
        addr: SocketAddr,
        tokens_path: &Path,
    ) -> Result<StoredCredentials<RedirectConnector>, Error> {
        StoredCredentials::load(
            redirected_client(addr),
            "me@example.com",
            &secret(),
            tokens_path,
        )
        .await
    }

    /// Bounds a call that would hang if it fell back to the interactive flow.
    async fn bounded<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("the token request did not finish; did it start an interactive flow?")
    }

    #[tokio::test]
    async fn valid_token_is_used_without_any_request() {
        let server = MockServer::start(vec![(500, String::from("{}"))]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(
            dir.path(),
            token("valid", Some("r"), TimeDuration::hours(1)),
        )
        .await;

        let credentials = load(server.addr(), &path).await.unwrap();
        let access = bounded(credentials.access_token(&DRIVE_SCOPES))
            .await
            .unwrap();

        assert_eq!(access.token(), Some("valid"));
        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn expired_token_is_refreshed_and_saved_with_the_old_refresh_token() {
        let server = MockServer::start(vec![(
            200,
            String::from(r#"{"access_token":"fresh","expires_in":3599,"token_type":"Bearer"}"#),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(
            dir.path(),
            token("stale", Some("refresh-1"), TimeDuration::hours(-1)),
        )
        .await;

        let credentials = load(server.addr(), &path).await.unwrap();
        // A subset of the stored scopes, as most Drive calls ask for.
        let access = bounded(credentials.access_token(&[DRIVE_SCOPES[1]]))
            .await
            .unwrap();
        assert_eq!(access.token(), Some("fresh"));

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        // yup-oauth2's authorized-user flow posts to https://accounts.google.com/o/oauth2/token.
        assert_eq!(requests[0].target, "/o/oauth2/token");
        assert_eq!(
            requests[0].form("grant_type").as_deref(),
            Some("refresh_token")
        );
        assert_eq!(
            requests[0].form("refresh_token").as_deref(),
            Some("refresh-1")
        );
        assert_eq!(
            requests[0].form("client_id").as_deref(),
            Some("client-id.apps.googleusercontent.com")
        );
        assert_eq!(
            requests[0].form("client_secret").as_deref(),
            Some("client-secret")
        );

        let saved = TokenFile::load(&path).await.unwrap().unwrap();
        assert_eq!(saved.refresh_token(), Some("refresh-1"));
        let saved_token = saved.get(&DRIVE_SCOPES[1..]).unwrap();
        assert_eq!(saved_token.access_token.as_deref(), Some("fresh"));
        assert!(!saved_token.is_expired());

        // The next run uses the saved token without another request.
        let again = load(server.addr(), &path).await.unwrap();
        let access = bounded(again.access_token(&DRIVE_SCOPES[1..]))
            .await
            .unwrap();
        assert_eq!(access.token(), Some("fresh"));
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn rejected_refresh_token_fails_fast_with_reauth_required() {
        let server = MockServer::start(vec![(
            400,
            String::from(
                r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
            ),
        )])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(
            dir.path(),
            token("stale", Some("dead"), TimeDuration::hours(-1)),
        )
        .await;
        let before = tokio::fs::read(&path).await.unwrap();

        let credentials = load(server.addr(), &path).await.unwrap();
        let error = bounded(credentials.access_token(&DRIVE_SCOPES))
            .await
            .unwrap_err();

        let Error::Reauth(reauth) = error else {
            panic!("expected ReauthRequired, got {error:?}");
        };
        assert_eq!(reauth.account, "me@example.com");
        assert_eq!(
            reauth.reason,
            ReauthReason::RefreshTokenRejected(Some(String::from(
                "Token has been expired or revoked."
            )))
        );
        assert_eq!(server.requests().len(), 1);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
    }

    #[tokio::test]
    async fn get_token_passes_reauth_required_to_the_drive_client() {
        use google_apis_common::GetToken;

        let server =
            MockServer::start(vec![(400, String::from(r#"{"error":"invalid_grant"}"#))]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(
            dir.path(),
            token("stale", Some("dead"), TimeDuration::hours(-1)),
        )
        .await;

        let credentials = load(server.addr(), &path).await.unwrap();
        let error = bounded(credentials.get_token(&DRIVE_SCOPES))
            .await
            .unwrap_err();
        let error = google_apis_common::Error::MissingToken(error);

        let reauth = crate::auth::find_reauth_required(&error).unwrap();
        assert_eq!(reauth.reason, ReauthReason::RefreshTokenRejected(None));
    }

    #[tokio::test]
    async fn other_refresh_errors_are_not_reported_as_reauth() {
        let server =
            MockServer::start(vec![(401, String::from(r#"{"error":"invalid_client"}"#))]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(
            dir.path(),
            token("stale", Some("r"), TimeDuration::hours(-1)),
        )
        .await;

        let credentials = load(server.addr(), &path).await.unwrap();
        let error = bounded(credentials.access_token(&DRIVE_SCOPES))
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Refresh(_)), "{error:?}");
    }

    #[tokio::test]
    async fn missing_or_empty_tokens_fail_without_any_request() {
        let server = MockServer::start(vec![(500, String::from("{}"))]).await;
        let dir = tempfile::tempdir().unwrap();

        let missing = dir.path().join("tokens.json");
        let error = load(server.addr(), &missing).await.err().unwrap();
        assert!(
            matches!(&error, Error::Reauth(r) if r.reason == ReauthReason::NoStoredToken),
            "{error:?}"
        );

        tokio::fs::write(&missing, "[]").await.unwrap();
        let error = load(server.addr(), &missing).await.err().unwrap();
        assert!(
            matches!(&error, Error::Reauth(r) if r.reason == ReauthReason::NoStoredToken),
            "{error:?}"
        );

        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn valid_token_without_refresh_token_works_until_it_expires() {
        let server = MockServer::start(vec![(500, String::from("{}"))]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(dir.path(), token("valid", None, TimeDuration::minutes(30))).await;

        let credentials = load(server.addr(), &path).await.unwrap();
        let access = bounded(credentials.access_token(&DRIVE_SCOPES[1..]))
            .await
            .unwrap();
        assert_eq!(access.token(), Some("valid"));

        // The Drive client gets it the same way.
        let token = bounded(google_apis_common::GetToken::get_token(
            &credentials,
            &DRIVE_SCOPES,
        ))
        .await
        .unwrap();
        assert_eq!(token.as_deref(), Some("valid"));

        assert!(server.requests().is_empty());
    }

    #[tokio::test]
    async fn expired_token_without_refresh_token_needs_a_new_authorization() {
        let server = MockServer::start(vec![(500, String::from("{}"))]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(dir.path(), token("stale", None, TimeDuration::hours(-1))).await;

        // Loading works; the error comes when a refresh would be needed.
        let credentials = load(server.addr(), &path).await.unwrap();
        let error = bounded(credentials.access_token(&DRIVE_SCOPES))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, Error::Reauth(r) if r.reason == ReauthReason::NoRefreshToken),
            "{error:?}"
        );
        assert!(server.requests().is_empty());
    }

    /// The answer of the token endpoint to a refresh: no refresh token, as Google sends it.
    const REFRESHED: &str = r#"{"access_token":"fresh","expires_in":3599,"token_type":"Bearer"}"#;

    #[tokio::test]
    async fn an_account_with_the_audit_log_works_for_drive_before_and_after_a_refresh() {
        let server = MockServer::start(vec![(200, String::from(REFRESHED))]).await;
        let dir = tempfile::tempdir().unwrap();

        let path = write_tokens_for(
            dir.path(),
            &DRIVE_AND_AUDIT,
            token("valid", Some("refresh-1"), TimeDuration::hours(1)),
        )
        .await;
        let credentials = load(server.addr(), &path).await.unwrap();
        for scopes in [&DRIVE_SCOPES[..], &DRIVE_SCOPES[..1], &DRIVE_SCOPES[1..]] {
            let access = bounded(credentials.access_token(scopes)).await.unwrap();
            assert_eq!(access.token(), Some("valid"), "{scopes:?}");
        }
        assert!(server.requests().is_empty());

        let path = write_tokens_for(
            dir.path(),
            &DRIVE_AND_AUDIT,
            token("stale", Some("refresh-1"), TimeDuration::hours(-1)),
        )
        .await;
        let credentials = load(server.addr(), &path).await.unwrap();
        let access = bounded(credentials.access_token(&DRIVE_SCOPES))
            .await
            .unwrap();
        assert_eq!(access.token(), Some("fresh"));
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].form("refresh_token").as_deref(),
            Some("refresh-1")
        );
        // The Drive calls that follow ask for one scope each: served by the refreshed token.
        let access = bounded(credentials.access_token(&DRIVE_SCOPES[..1]))
            .await
            .unwrap();
        assert_eq!(access.token(), Some("fresh"));
        assert_eq!(server.requests().len(), 1);

        // The refresh kept the record of the grant and the refresh token.
        let saved = TokenFile::load(&path).await.unwrap().unwrap();
        assert!(saved.audit_log_granted());
        assert_eq!(saved.refresh_token(), Some("refresh-1"));
        let drive = saved.get(&DRIVE_SCOPES).unwrap();
        assert_eq!(drive.access_token.as_deref(), Some("fresh"));
        assert_eq!(drive.refresh_token.as_deref(), Some("refresh-1"));

        // The next run uses the saved token.
        let again = load(server.addr(), &path).await.unwrap();
        let access = bounded(again.access_token(&DRIVE_SCOPES)).await.unwrap();
        assert_eq!(access.token(), Some("fresh"));
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn an_audit_log_token_is_cached_while_valid_and_refreshed_when_expired() {
        let server = MockServer::start(vec![(200, String::from(REFRESHED))]).await;
        let dir = tempfile::tempdir().unwrap();

        let path = write_tokens_for(
            dir.path(),
            &DRIVE_AND_AUDIT,
            token("valid", Some("refresh-1"), TimeDuration::hours(1)),
        )
        .await;
        let credentials = load(server.addr(), &path).await.unwrap();
        let access = bounded(credentials.access_token(&[AUDIT_LOG_SCOPE]))
            .await
            .unwrap();
        assert_eq!(access.token(), Some("valid"));
        assert!(server.requests().is_empty());

        let path = write_tokens_for(
            dir.path(),
            &DRIVE_AND_AUDIT,
            token("stale", Some("refresh-1"), TimeDuration::hours(-1)),
        )
        .await;
        let credentials = load(server.addr(), &path).await.unwrap();
        for _ in 0..2 {
            let access = bounded(credentials.access_token(&[AUDIT_LOG_SCOPE]))
                .await
                .unwrap();
            assert_eq!(access.token(), Some("fresh"));
        }
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].form("grant_type").as_deref(),
            Some("refresh_token")
        );
        assert_eq!(
            requests[0].form("refresh_token").as_deref(),
            Some("refresh-1")
        );

        let saved = TokenFile::load(&path).await.unwrap().unwrap();
        assert!(saved.audit_log_granted());
        assert_eq!(saved.refresh_token(), Some("refresh-1"));
        let again = load(server.addr(), &path).await.unwrap();
        let access = bounded(again.access_token(&[AUDIT_LOG_SCOPE]))
            .await
            .unwrap();
        assert_eq!(access.token(), Some("fresh"));
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn an_account_without_the_grant_never_gets_an_audit_log_entry() {
        let server = MockServer::start(vec![(200, String::from(REFRESHED))]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(
            dir.path(),
            token("valid", Some("refresh-1"), TimeDuration::hours(1)),
        )
        .await;
        let before = tokio::fs::read(&path).await.unwrap();

        // Asked for the audit log anyway (`get_audit_hub_for` checks the grant first and never
        // does this), yup-oauth2 finds no entry for it and refreshes ...
        let credentials = load(server.addr(), &path).await.unwrap();
        bounded(credentials.access_token(&[AUDIT_LOG_SCOPE]))
            .await
            .unwrap();
        assert_eq!(server.requests().len(), 1);

        // ... but nothing is stored that claims the grant.
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert!(!TokenFile::load(&path)
            .await
            .unwrap()
            .unwrap()
            .audit_log_granted());
    }
}
