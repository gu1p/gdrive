//! Credentials for the Drive API.
//!
//! Only the login commands (`account add`, `account reauth`) may ask the user for an
//! authorization. Every other command goes through [`stored::StoredCredentials`], which can
//! only use and refresh the token saved in `tokens.json` and fails with [`ReauthRequired`]
//! when that is not possible.

pub(crate) mod adc;
pub(crate) mod login;
pub(crate) mod service;
pub(crate) mod store;
pub(crate) mod stored;

#[cfg(test)]
pub(crate) mod test_support;

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
};

/// The scopes gdrive asks for when an account is authorized. Tokens are stored under this
/// scope set in `tokens.json`.
pub(crate) const DRIVE_SCOPES: [&str; 2] = [
    "https://www.googleapis.com/auth/drive",
    "https://www.googleapis.com/auth/drive.metadata.readonly",
];

/// The stored authorization of an account cannot be used any more; only a new login helps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReauthRequired {
    pub(crate) account: String,
    pub(crate) reason: ReauthReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReauthReason {
    /// `tokens.json` is missing or holds no token.
    NoStoredToken,
    /// The cached access token expired and no stored token has a refresh token (for example
    /// after Google omitted it, #150).
    NoRefreshToken,
    /// Google answered the refresh request with `invalid_grant`: the refresh token expired or
    /// was revoked. Holds the server's `error_description`, if any.
    RefreshTokenRejected(Option<String>),
}

impl Display for ReauthRequired {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let account = &self.account;
        write!(f, "account '{account}' needs to be authorized again: ")?;
        match &self.reason {
            ReauthReason::NoStoredToken => f.write_str("no stored token was found")?,
            ReauthReason::NoRefreshToken => {
                f.write_str("the stored access token expired and no refresh token is stored")?;
            }
            ReauthReason::RefreshTokenRejected(description) => {
                f.write_str("Google rejected the stored refresh token (invalid_grant")?;
                if let Some(description) = description {
                    write!(f, ": {description}")?;
                }
                f.write_str(")")?;
            }
        }
        write!(
            f,
            "; run 'gdrive account add' (or 'gdrive account reauth {account}') to authorize again"
        )?;
        if matches!(self.reason, ReauthReason::RefreshTokenRejected(_)) {
            f.write_str(
                "\nHint: if the publishing status of your OAuth app is 'Testing', Google expires \
                 its refresh tokens after 7 days; set it to 'In production' to keep unattended \
                 runs working",
            )?;
        }
        Ok(())
    }
}

impl Error for ReauthRequired {}

/// Warns when the tokens of a fresh authorization have no refresh token: the account would
/// stop working when the access token expires.
pub(crate) fn warn_if_no_refresh_token(tokens: &store::TokenFile, account: &str) {
    if tokens.refresh_token().is_none() {
        eprintln!(
            "Warning: Google returned no refresh token for '{account}', so gdrive will stop \
             working when the access token expires (in about an hour). Remove gdrive's access at \
             https://myaccount.google.com/permissions, then run 'gdrive account reauth {account}'"
        );
    }
}

/// Finds a [`ReauthRequired`] in an error chain.
///
/// The Drive API client wraps token errors in `MissingToken`, whose `source()` returns `None`,
/// so that variant is looked into explicitly.
pub(crate) fn find_reauth_required<'a>(
    error: &'a (dyn Error + 'static),
) -> Option<&'a ReauthRequired> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(reauth) = error.downcast_ref::<ReauthRequired>() {
            return Some(reauth);
        }
        if let Some(google_apis_common::Error::MissingToken(inner)) =
            error.downcast_ref::<google_apis_common::Error>()
        {
            let inner: &(dyn Error + 'static) = inner.as_ref();
            if let Some(reauth) = find_reauth_required(inner) {
                return Some(reauth);
            }
        }
        current = error.source();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reauth(reason: ReauthReason) -> ReauthRequired {
        ReauthRequired {
            account: String::from("me@example.com"),
            reason,
        }
    }

    #[test]
    fn display_names_the_commands_to_run() {
        let message = reauth(ReauthReason::NoRefreshToken).to_string();
        assert_eq!(
            message,
            "account 'me@example.com' needs to be authorized again: the stored access token \
             expired and no refresh token is stored; run 'gdrive account add' (or 'gdrive \
             account reauth me@example.com') to authorize again"
        );

        let message = reauth(ReauthReason::RefreshTokenRejected(Some(String::from(
            "Token has been expired or revoked.",
        ))))
        .to_string();
        assert!(message.starts_with(
            "account 'me@example.com' needs to be authorized again: Google rejected the stored \
             refresh token (invalid_grant: Token has been expired or revoked.); run 'gdrive \
             account add'"
        ));
        assert!(message.contains("7 days"));
    }

    #[derive(Debug)]
    struct Outer(Box<dyn Error + 'static>);

    impl Display for Outer {
        fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
            f.write_str("outer")
        }
    }

    impl Error for Outer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            Some(self.0.as_ref())
        }
    }

    #[test]
    fn finds_reauth_in_source_chain_and_inside_missing_token() {
        let expected = reauth(ReauthReason::NoStoredToken);

        let direct = Outer(Box::new(expected.clone()));
        assert_eq!(find_reauth_required(&direct), Some(&expected));

        let wrapped = Outer(Box::new(google_apis_common::Error::MissingToken(Box::new(
            expected.clone(),
        ))));
        assert_eq!(find_reauth_required(&wrapped), Some(&expected));

        let unrelated = Outer(Box::new(google_apis_common::Error::Cancelled));
        assert_eq!(find_reauth_required(&unrelated), None);
    }
}
