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

    /// A refresh answer with the access token `access`. Valid for 30 seconds, a token counts as
    /// expired at once (yup-oauth2 renews it a minute early), so the next request refreshes
    /// again.
    fn refresh_answer(access: &str, expires_in: u32) -> (u16, String) {
        (
            200,
            format!(
                r#"{{"access_token":"{access}","expires_in":{expires_in},"token_type":"Bearer"}}"#
            ),
        )
    }

    fn last_refresh_token(server: &MockServer) -> Option<String> {
        server.requests().last()?.form("refresh_token")
    }

    /// Runs a command of the authorization in `tokens_path` (refresh token `refresh-1`, for
    /// `loaded_scopes`) while the account is authorized again for `login_scopes` (refresh token
    /// `refresh-2`). The older command refreshes after the new login was saved, and with
    /// `refreshes_first` also before. Checks that nothing of it reaches the new file, and
    /// returns that file's content.
    async fn older_command_meets_a_new_login(
        server: &MockServer,
        dir: &Path,
        loaded_scopes: &[&str],
        login_scopes: &[&str],
        refreshes_first: bool,
    ) -> Vec<u8> {
        let path = write_tokens_for(
            dir,
            loaded_scopes,
            token("stale", Some("refresh-1"), TimeDuration::hours(-1)),
        )
        .await;
        let older = load(server.addr(), &path).await.unwrap();
        let asked = loaded_scopes;
        if refreshes_first {
            bounded(older.access_token(asked)).await.unwrap();
            assert_eq!(last_refresh_token(server).as_deref(), Some("refresh-1"));
        }

        let previous = TokenFile::load(&path).await.unwrap();
        TokenFile::after_login(
            previous.as_ref(),
            login_scopes,
            token("login", Some("refresh-2"), TimeDuration::hours(-1)),
        )
        .save(&path)
        .await
        .unwrap();
        let saved = tokio::fs::read(&path).await.unwrap();

        // The older command refreshes with its own refresh token and keeps the token to itself.
        let requests = server.requests().len();
        let access = bounded(older.access_token(asked)).await.unwrap();
        assert_eq!(access.token(), Some("older"));
        assert_eq!(server.requests().len(), requests + 1);
        assert_eq!(last_refresh_token(server).as_deref(), Some("refresh-1"));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), saved);
        // It goes on working from memory, without another request.
        for scopes in [asked, &DRIVE_SCOPES[..1]] {
            let access = bounded(older.access_token(scopes)).await.unwrap();
            assert_eq!(access.token(), Some("older"), "{scopes:?}");
        }
        assert_eq!(server.requests().len(), requests + 1);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), saved);
        saved
    }

    /// The answers of the token endpoint for [`older_command_meets_a_new_login`] and one more
    /// refresh, `next`, of a command started after the login.
    async fn token_endpoint(refreshes_first: bool, next: &str) -> MockServer {
        let mut answers = Vec::new();
        if refreshes_first {
            answers.push(refresh_answer("older-early", 30));
        }
        answers.push(refresh_answer("older", 3599));
        answers.push(refresh_answer(next, 3599));
        MockServer::start(answers).await
    }

    #[tokio::test]
    async fn an_older_drive_command_cannot_undo_an_audit_log_reauthorization() {
        for refreshes_first in [false, true] {
            let server = token_endpoint(refreshes_first, "audit").await;
            let dir = tempfile::tempdir().unwrap();
            // A Drive-only command, `gdrive account reauth --audit-log`, the command refreshes.
            older_command_meets_a_new_login(
                &server,
                dir.path(),
                &DRIVE_SCOPES,
                &DRIVE_AND_AUDIT,
                refreshes_first,
            )
            .await;

            // Then a command that reads the audit log refreshes, with the new refresh token.
            let path = dir.path().join("tokens.json");
            let audit = load(server.addr(), &path).await.unwrap();
            assert!(audit.audit_log_granted());
            let access = bounded(audit.access_token(&[AUDIT_LOG_SCOPE]))
                .await
                .unwrap();
            assert_eq!(access.token(), Some("audit"));
            assert_eq!(last_refresh_token(&server).as_deref(), Some("refresh-2"));

            let saved = TokenFile::load(&path).await.unwrap().unwrap();
            assert!(saved.audit_log_granted());
            assert_eq!(saved.refresh_token(), Some("refresh-2"));
            let json = tokio::fs::read_to_string(&path).await.unwrap();
            assert!(!json.contains("refresh-1"), "{refreshes_first}: {json}");
        }
    }

    #[tokio::test]
    async fn an_older_audit_log_command_cannot_undo_a_drive_only_reauthorization() {
        for refreshes_first in [false, true] {
            let server = token_endpoint(refreshes_first, "drive").await;
            let dir = tempfile::tempdir().unwrap();
            // An audit-log command (it asks for the scopes of the grant, as `get_audit_hub_for`
            // does), a reauthorization without the audit log, the command refreshes.
            older_command_meets_a_new_login(
                &server,
                dir.path(),
                &DRIVE_AND_AUDIT,
                &DRIVE_SCOPES,
                refreshes_first,
            )
            .await;

            let path = dir.path().join("tokens.json");
            let drive = load(server.addr(), &path).await.unwrap();
            assert!(!drive.audit_log_granted());
            let access = bounded(drive.access_token(&DRIVE_SCOPES)).await.unwrap();
            assert_eq!(access.token(), Some("drive"));
            assert_eq!(last_refresh_token(&server).as_deref(), Some("refresh-2"));
            let saved = TokenFile::load(&path).await.unwrap().unwrap();
            assert!(!saved.audit_log_granted());
            assert_eq!(saved.refresh_token(), Some("refresh-2"));
        }
    }

    #[tokio::test]
    async fn an_older_command_never_writes_its_refresh_token_over_a_newer_one() {
        // Plain reauthorizations, for Drive only and with the audit log: the earlier refresh
        // token may have been revoked, and must not come back.
        for scopes in [&DRIVE_SCOPES[..], &DRIVE_AND_AUDIT] {
            for refreshes_first in [false, true] {
                let server = token_endpoint(refreshes_first, "new").await;
                let dir = tempfile::tempdir().unwrap();
                older_command_meets_a_new_login(
                    &server,
                    dir.path(),
                    scopes,
                    scopes,
                    refreshes_first,
                )
                .await;

                let path = dir.path().join("tokens.json");
                let new = load(server.addr(), &path).await.unwrap();
                let access = bounded(new.access_token(&DRIVE_SCOPES[..1])).await.unwrap();
                assert_eq!(access.token(), Some("new"));
                assert_eq!(last_refresh_token(&server).as_deref(), Some("refresh-2"));
                let json = tokio::fs::read_to_string(&path).await.unwrap();
                assert!(!json.contains("refresh-1"), "{scopes:?}: {json}");
            }
        }
    }

    #[tokio::test]
    async fn an_account_that_is_not_authorized_again_keeps_its_layout_and_writes_only_to_refresh() {
        let server = MockServer::start(vec![refresh_answer("fresh", 3599)]).await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens(
            dir.path(),
            token("stale", Some("refresh-1"), TimeDuration::hours(-1)),
        )
        .await;

        let credentials = load(server.addr(), &path).await.unwrap();
        bounded(credentials.access_token(&DRIVE_SCOPES))
            .await
            .unwrap();

        // The layout of gdrive 3.x: one entry, the same keys.
        let json: serde_json::Value =
            serde_json::from_slice(&tokio::fs::read(&path).await.unwrap()).unwrap();
        let entries = json.as_array().unwrap();
        assert_eq!(entries.len(), 1);
        let keys = |value: &serde_json::Value| {
            let mut keys: Vec<_> = value.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            keys
        };
        assert_eq!(keys(&entries[0]), ["scopes", "token"]);
        assert_eq!(
            keys(&entries[0]["token"]),
            ["access_token", "expires_at", "id_token", "refresh_token"]
        );
        assert_eq!(entries[0]["scopes"], serde_json::json!(DRIVE_SCOPES));
        assert_eq!(entries[0]["token"]["refresh_token"], "refresh-1");

        // Served from the stored token, in this process and the next: no more writes.
        let written = tokio::fs::read(&path).await.unwrap();
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&path).unwrap());
        let again = load(server.addr(), &path).await.unwrap();
        for credentials in [&credentials, &again] {
            for scopes in [&DRIVE_SCOPES[..], &DRIVE_SCOPES[..1], &DRIVE_SCOPES[1..]] {
                let access = bounded(credentials.access_token(scopes)).await.unwrap();
                assert_eq!(access.token(), Some("fresh"));
            }
        }
        assert_eq!(server.requests().len(), 1);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), written);
        #[cfg(unix)]
        assert_eq!(
            std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&path).unwrap()),
            inode
        );
    }

    #[tokio::test]
    async fn a_refresh_token_in_a_refresh_answer_is_saved_and_followed_by_later_refreshes() {
        // Each answer counts as expired at once, so every request refreshes again.
        let answer = |access: &str, refresh: Option<&str>| {
            let mut answer = serde_json::json!({
                "access_token": access,
                "expires_in": 30,
                "token_type": "Bearer",
            });
            if let Some(refresh) = refresh {
                answer["refresh_token"] = refresh.into();
            }
            (200, answer.to_string())
        };
        let server = MockServer::start(vec![
            answer("a", Some("refresh-2")),
            answer("b", Some("refresh-3")),
            answer("c", None),
            answer("d", None),
        ])
        .await;
        let dir = tempfile::tempdir().unwrap();
        let path = write_tokens_for(
            dir.path(),
            &DRIVE_AND_AUDIT,
            token("stale", Some("refresh-1"), TimeDuration::hours(-1)),
        )
        .await;

        let credentials = load(server.addr(), &path).await.unwrap();
        for (access, refresh_token) in [("a", "refresh-2"), ("b", "refresh-3"), ("c", "refresh-3")]
        {
            let token = bounded(credentials.access_token(&DRIVE_SCOPES))
                .await
                .unwrap();
            assert_eq!(token.token(), Some(access));
            let saved = TokenFile::load(&path).await.unwrap().unwrap();
            assert_eq!(saved.refresh_token(), Some(refresh_token), "after {access}");
            assert_eq!(
                saved.get(&DRIVE_SCOPES).unwrap().access_token.as_deref(),
                Some(access)
            );
            assert!(saved.audit_log_granted(), "after {access}");
        }
        // yup-oauth2 keeps refreshing with the refresh token this process started with.
        let sent: Vec<_> = server
            .requests()
            .iter()
            .filter_map(|request| request.form("refresh_token"))
            .collect();
        assert_eq!(sent, ["refresh-1"; 3]);

        // The next process refreshes with the latest one.
        let next = load(server.addr(), &path).await.unwrap();
        let token = bounded(next.access_token(&DRIVE_SCOPES)).await.unwrap();
        assert_eq!(token.token(), Some("d"));
        assert_eq!(last_refresh_token(&server).as_deref(), Some("refresh-3"));
    }
}
