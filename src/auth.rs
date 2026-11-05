//! Credentials for the Drive API.
//!
//! Only the login commands (`account add`, `account reauth`) may ask the user for an
//! authorization. Every other command goes through [`stored::StoredCredentials`], which can
//! only use and refresh the token saved in `tokens.json` and fails with [`ReauthRequired`]
//! when that is not possible.
//!
//! An account is authorized for [`DRIVE_SCOPES`], and with `--audit-log` of the login commands
//! also for [`AUDIT_LOG_SCOPE`]. `tokens.json` records what Google granted (see
//! [`store::TokenFile::audit_log_granted`]).

pub(crate) mod adc;
pub(crate) mod lock;
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

/// Read access to the audit log of a Google Workspace organization (Admin SDK Reports API), which
/// `gdrive files activity` needs. Asked for only with `--audit-log` of the login commands.
pub(crate) const AUDIT_LOG_SCOPE: &str =
    "https://www.googleapis.com/auth/admin.reports.audit.readonly";

/// The scopes of a login with `--audit-log`: [`DRIVE_SCOPES`] and [`AUDIT_LOG_SCOPE`]. When Google
/// granted both, the token is stored under this scope set.
pub(crate) const DRIVE_AND_AUDIT_LOG_SCOPES: [&str; 3] =
    [DRIVE_SCOPES[0], DRIVE_SCOPES[1], AUDIT_LOG_SCOPE];

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

/// Why a login took away the account's read access to the audit log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuditLogLoss {
    /// The login did not ask for it (`account add` without `--audit-log`).
    NotAsked,
    /// The login asked for it again and Google did not grant it.
    NotGranted,
    /// Google granted it but returned no refresh token, and the stored one was not issued with
    /// it (see `login::tokens_after_login`).
    NoRefreshToken,
}

/// The warning for a login that took away the account's read access to the audit log: the
/// account had it, and the new authorization, which replaces the old one, does not include it.
pub(crate) fn audit_log_lost_warning(account: &str, loss: AuditLogLoss) -> String {
    match loss {
        AuditLogLoss::NotAsked => format!(
            "Warning: account '{account}' could read the audit log of its organization, and this \
             login did not ask for that access, so 'gdrive files activity' no longer works with \
             it. To get the access back, run 'gdrive account reauth {account} --audit-log'"
        ),
        AuditLogLoss::NotGranted => format!(
            "Warning: Google did not grant read access to the audit log this time, so 'gdrive \
             files activity' no longer works with account '{account}'; its Drive authorization \
             was saved. To get the access back, run 'gdrive account reauth {account} \
             --audit-log' and leave the box for the audit log ticked (the Google account must be \
             an administrator of the organization)"
        ),
        AuditLogLoss::NoRefreshToken => format!(
            "Warning: Google returned no refresh token, and the stored one does not include read \
             access to the audit log, so 'gdrive files activity' no longer works with account \
             '{account}'; its Drive authorization was saved. To get the access back, remove \
             gdrive's access at https://myaccount.google.com/permissions, then run 'gdrive \
             account reauth {account} --audit-log'"
        ),
    }
}

/// Why the login of `account`, which had the audit log, took it away; `None` when the new
/// tokens record the grant too.
pub(crate) fn audit_log_loss(
    asked: login::AuditLog,
    granted_by_google: bool,
    recorded: bool,
) -> Option<AuditLogLoss> {
    if recorded {
        None
    } else if asked == login::AuditLog::No {
        Some(AuditLogLoss::NotAsked)
    } else if granted_by_google {
        Some(AuditLogLoss::NoRefreshToken)
    } else {
        Some(AuditLogLoss::NotGranted)
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

    #[test]
    fn the_audit_log_warnings_name_the_command_that_brings_the_access_back() {
        use AuditLogLoss::{NoRefreshToken, NotAsked, NotGranted};

        for loss in [NotAsked, NotGranted, NoRefreshToken] {
            let warning = audit_log_lost_warning("me@example.com", loss);
            assert!(warning.starts_with("Warning: "), "{warning}");
            assert!(
                warning.contains("'gdrive account reauth me@example.com --audit-log'"),
                "{warning}"
            );
            assert!(warning.contains("'gdrive files activity'"), "{warning}");
        }
        for loss in [NotGranted, NoRefreshToken] {
            assert!(audit_log_lost_warning("me@example.com", loss)
                .contains("its Drive authorization was saved"));
        }
        assert!(audit_log_lost_warning("me@example.com", NoRefreshToken)
            .contains("https://myaccount.google.com/permissions"));

        let asked = login::AuditLog::Optional;
        assert_eq!(audit_log_loss(asked, true, true), None);
        assert_eq!(audit_log_loss(asked, false, false), Some(NotGranted));
        assert_eq!(audit_log_loss(asked, true, false), Some(NoRefreshToken));
        assert_eq!(
            audit_log_loss(login::AuditLog::No, false, false),
            Some(NotAsked)
        );
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
