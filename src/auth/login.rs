//! The OAuth authorization code flow of `account add` and `account reauth`.
//!
//! gdrive builds this flow itself instead of using yup-oauth2's installed flow, which sends
//! neither PKCE nor `state` and cannot take the redirect from the user. Google blocked the
//! out-of-band flow, so the redirect URI is the loopback address `http://localhost:PORT` in
//! both modes:
//!
//! - browser mode listens on `127.0.0.1:PORT` (and `[::1]:PORT` when possible) for the
//!   browser's redirect;
//! - manual mode prints the URL and reads the address of the page the browser was redirected
//!   to, from any machine. That page does not load, but its address holds `code` and `state`.
//!
//! The authorization URL carries a random `state`, which the redirect must return, and a PKCE
//! S256 `code_challenge`, whose `code_verifier` goes with the code to the token endpoint.
//!
//! The login asks for [`DRIVE_SCOPES`], and with [`AuditLog::Required`] or [`AuditLog::Optional`]
//! also for [`AUDIT_LOG_SCOPE`]. The `scope` field of the token response says what the user
//! granted: the consent screen lets them untick a scope.

use std::{
    error,
    fmt::{self, Display, Formatter},
    io::{self, Write},
    net::{Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use http_body_util::BodyExt;
use hyper::{header, Request, StatusCode};
use hyper_util::client::legacy::{connect::Connect, Client};
use ring::rand::{SecureRandom, SystemRandom};
use serde::Deserialize;
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::mpsc,
    task::JoinSet,
};
use url::Url;
use yup_oauth2::storage::TokenInfo;

use super::{AUDIT_LOG_SCOPE, DRIVE_AND_AUDIT_LOG_SCOPES, DRIVE_SCOPES};
use crate::{app_config::Secret, hub};

/// The default port of the loopback redirect URI, as in gdrive 3.x (SSH tunnels rely on it).
pub(crate) const DEFAULT_PORT: u16 = 8085;

const GOOGLE_AUTH_URI: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";

/// How long browser mode waits for the redirect.
const REDIRECT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// How long a connection to the redirect listener may take to send its request.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the token endpoint may take to answer.
const TOKEN_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
/// The largest request head the redirect listener reads.
const MAX_REQUEST_HEAD: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Listen on the loopback port for the browser's redirect.
    Browser,
    /// Print the URL and read the redirect URL from stdin.
    Manual,
}

/// Whether a login also asks for read access to the audit log ([`AUDIT_LOG_SCOPE`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum AuditLog {
    /// The Drive scopes only.
    #[default]
    No,
    /// `--audit-log`: the login fails with [`LoginError::AuditLogNotGranted`] when Google does not
    /// grant it.
    Required,
    /// `account reauth` of an account that has the access, without `--audit-log`: asked for
    /// again, and when Google does not grant it the login succeeds with the Drive scopes only.
    Optional,
}

impl AuditLog {
    /// The scopes the authorization URL asks for.
    fn scopes(self) -> &'static [&'static str] {
        match self {
            AuditLog::No => &DRIVE_SCOPES,
            AuditLog::Required | AuditLog::Optional => &DRIVE_AND_AUDIT_LOG_SCOPES,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) mode: Mode,
    /// Port of the redirect URI. In browser mode, 0 picks a free port.
    pub(crate) port: u16,
    /// Preselects the Google account on the consent screen.
    pub(crate) login_hint: Option<String>,
    audit_log: AuditLog,
    endpoints: Endpoints,
}

/// The OAuth endpoints. Not configurable from the command line; tests point them at mocks.
#[derive(Debug, Clone)]
struct Endpoints {
    auth_uri: String,
    token_uri: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Endpoints {
            auth_uri: String::from(GOOGLE_AUTH_URI),
            token_uri: String::from(GOOGLE_TOKEN_URI),
        }
    }
}

impl Options {
    pub(crate) fn new(mode: Mode, port: u16, login_hint: Option<String>) -> Self {
        Options {
            mode,
            port,
            login_hint,
            audit_log: AuditLog::No,
            endpoints: Endpoints::default(),
        }
    }

    /// Whether the login also asks for read access to the audit log.
    #[must_use]
    pub(crate) fn with_audit_log(mut self, audit_log: AuditLog) -> Self {
        self.audit_log = audit_log;
        self
    }

    /// Rejects option combinations that cannot work, before the user is asked anything.
    pub(crate) fn check(&self) -> Result<(), LoginError> {
        if self.mode == Mode::Manual && self.port == 0 {
            return Err(LoginError::ManualNeedsPort);
        }
        Ok(())
    }

    #[cfg(test)]
    fn with_endpoints(mut self, auth_uri: String, token_uri: String) -> Self {
        self.endpoints = Endpoints {
            auth_uri,
            token_uri,
        };
        self
    }
}

/// What a login got from Google.
#[derive(Debug)]
pub(crate) struct Authorization {
    pub(crate) token: TokenInfo,
    /// Whether Google granted read access to the audit log. Only when the login asked for it;
    /// a token response without a `scope` field grants what was asked for.
    pub(crate) audit_log: bool,
}

impl Authorization {
    /// The scopes to store the token for in `tokens.json`: [`DRIVE_SCOPES`], and
    /// [`AUDIT_LOG_SCOPE`] too when Google granted it.
    pub(crate) fn scopes(&self) -> &'static [&'static str] {
        if self.audit_log {
            &DRIVE_AND_AUDIT_LOG_SCOPES
        } else {
            &DRIVE_SCOPES
        }
    }
}

/// Asks the user to authorize gdrive and returns what Google granted.
pub(crate) async fn login(secret: &Secret, options: &Options) -> Result<Authorization, LoginError> {
    options.check()?;
    let (code, request) = match options.mode {
        Mode::Browser => {
            // Bind first: a busy port is reported before the user opens the URL.
            let listener = RedirectListener::bind(options.port).await?;
            let request = AuthorizationRequest::new(
                &options.endpoints.auth_uri,
                &secret.client_id,
                &format!("http://localhost:{}", listener.port()),
                options.login_hint.as_deref(),
                options.audit_log,
            )?;
            print_browser_instructions(&request, listener.port());
            let code = listener.wait_for_code(&request, REDIRECT_TIMEOUT).await?;
            (code, request)
        }
        Mode::Manual => {
            let request = AuthorizationRequest::new(
                &options.endpoints.auth_uri,
                &secret.client_id,
                &format!("http://localhost:{}", options.port),
                options.login_hint.as_deref(),
                options.audit_log,
            )?;
            print_manual_instructions(&request, options.port);
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
            let code = read_code(&request, &mut stdin).await?;
            (code, request)
        }
    };

    let client = hub::token_http_client().map_err(LoginError::HttpClient)?;
    exchange_code(
        &client,
        &options.endpoints.token_uri,
        secret,
        &request,
        &code,
    )
    .await
}

fn print_browser_instructions(request: &AuthorizationRequest, port: u16) {
    println!();
    println!("Open this URL in a browser and allow gdrive to access your Google Drive:");
    println!();
    println!("{}", request.url());
    println!();
    print_audit_log_note(request);
    println!(
        "gdrive is waiting for the browser to be redirected to http://localhost:{port}. That \
         works when the browser runs on this machine or reaches it through an SSH tunnel to port \
         {port}. If the browser runs on another machine, stop gdrive and run the command again \
         with --manual."
    );
}

fn print_manual_instructions(request: &AuthorizationRequest, port: u16) {
    println!();
    println!(
        "Open this URL in a browser on any machine and allow gdrive to access your Google Drive:"
    );
    println!();
    println!("{}", request.url());
    println!();
    print_audit_log_note(request);
    println!(
        "Google then redirects the browser to http://localhost:{port}/?code=... and the page \
         fails to load; that is expected. Copy the full address from the browser's address bar \
         and paste it here."
    );
}

/// Tells the user that the consent screen asks for the audit log too, when it does.
fn print_audit_log_note(request: &AuthorizationRequest) {
    if request.audit_log != AuditLog::No {
        println!(
            "gdrive also asks for read access to the audit log of your Google Workspace \
             organization. Leave that box ticked on the consent screen."
        );
        println!();
    }
}

/// One authorization attempt: the URL to open and the secrets the redirect is checked with.
pub(crate) struct AuthorizationRequest {
    url: String,
    redirect_uri: String,
    state: String,
    code_verifier: String,
    /// Whether the URL asks for the audit log, and what the answer must grant.
    audit_log: AuditLog,
}

impl AuthorizationRequest {
    fn new(
        auth_uri: &str,
        client_id: &str,
        redirect_uri: &str,
        login_hint: Option<&str>,
        audit_log: AuditLog,
    ) -> Result<Self, LoginError> {
        let random = SystemRandom::new();
        let state = random_token(&random)?;
        let code_verifier = random_token(&random)?;
        let code_challenge = URL_SAFE_NO_PAD.encode(ring::digest::digest(
            &ring::digest::SHA256,
            code_verifier.as_bytes(),
        ));

        let mut url = Url::parse(auth_uri).map_err(|_| LoginError::InvalidEndpoint)?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("client_id", client_id)
                .append_pair("redirect_uri", redirect_uri)
                .append_pair("response_type", "code")
                .append_pair("scope", &audit_log.scopes().join(" "))
                .append_pair("access_type", "offline")
                // Google issues a refresh token only when the consent screen is shown (#150).
                .append_pair("prompt", "consent")
                .append_pair("state", &state)
                .append_pair("code_challenge", &code_challenge)
                .append_pair("code_challenge_method", "S256");
            if let Some(login_hint) = login_hint {
                query.append_pair("login_hint", login_hint);
            }
        }

        Ok(AuthorizationRequest {
            url: url.into(),
            redirect_uri: redirect_uri.to_owned(),
            state,
            code_verifier,
            audit_log,
        })
    }

    pub(crate) fn url(&self) -> &str {
        &self.url
    }

    /// The code from the address the browser was redirected to, as the user pasted it: a full
    /// URL, one without scheme, or only its query.
    fn code_from_pasted(&self, pasted: &str) -> Result<String, LoginError> {
        let pasted = pasted
            .trim()
            .trim_matches(|c| matches!(c, '"' | '\'' | '<' | '>'));
        let url = if pasted.contains("://") {
            Url::parse(pasted)
        } else if let Some((_, query)) = pasted.split_once('?') {
            Url::parse(&format!("http://localhost/?{query}"))
        } else {
            Url::parse(&format!("http://localhost/?{pasted}"))
        };
        let url = url.map_err(|_| LoginError::NotARedirect)?;
        code_from_query(&url, &self.state).ok_or(LoginError::NotARedirect)?
    }
}

/// 32 random bytes, base64url without padding: 43 characters, valid both as `state` and as a
/// PKCE `code_verifier` (RFC 7636 section 4.1).
fn random_token(random: &SystemRandom) -> Result<String, LoginError> {
    let mut bytes = [0; 32];
    random.fill(&mut bytes).map_err(|_| LoginError::Random)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// Reads the result of an authorization from a redirect URL. `None` when the URL carries
/// neither `code` nor `error`, so it is not a redirect from Google.
fn code_from_query(url: &Url, expected_state: &str) -> Option<Result<String, LoginError>> {
    let mut code = None;
    let mut state = None;
    let mut error = None;
    let mut error_description = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => error = Some(value.into_owned()),
            "error_description" => error_description = Some(value.into_owned()),
            _ => {}
        }
    }

    if code.is_none() && error.is_none() {
        return None;
    }
    // Answers and refusals both carry the state of the request they belong to. Another state
    // means another login: an earlier one (an old tab reloaded) or somebody else's.
    if state.as_deref() != Some(expected_state) {
        return Some(Err(LoginError::StateMismatch));
    }
    if let Some(error) = error {
        return Some(Err(LoginError::Denied {
            error,
            description: error_description,
        }));
    }
    code.map(Ok)
}

/// Reads pasted redirect URLs from `input` until one holds the code.
async fn read_code<R>(request: &AuthorizationRequest, input: &mut R) -> Result<String, LoginError>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        print!("Redirect URL: ");
        let _ = io::stdout().flush();

        let mut line = String::new();
        let read = input
            .read_line(&mut line)
            .await
            .map_err(LoginError::ReadInput)?;
        if read == 0 {
            return Err(LoginError::InputClosed);
        }
        if line.trim().is_empty() {
            continue;
        }

        match request.code_from_pasted(&line) {
            Err(LoginError::NotARedirect) => println!(
                "That is not the address of the redirect: it must contain code= and state=. \
                 Paste the full address of the page the browser was sent to after the approval."
            ),
            Err(LoginError::StateMismatch) => println!(
                "That address belongs to another login (its state does not match). Open the URL \
                 printed above, approve, and paste the address the browser lands on."
            ),
            result => return result,
        }
    }
}

/// The loopback listener of browser mode.
struct RedirectListener {
    v4: TcpListener,
    v6: Option<TcpListener>,
    port: u16,
}

impl RedirectListener {
    async fn bind(port: u16) -> Result<Self, LoginError> {
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|source| {
                if source.kind() == io::ErrorKind::AddrInUse {
                    LoginError::PortInUse(port)
                } else {
                    LoginError::Listen { port, source }
                }
            })?;
        let port = v4
            .local_addr()
            .map_err(|source| LoginError::Listen { port, source })?
            .port();
        // `localhost` may resolve to ::1 first. Without IPv6, the browser falls back to IPv4.
        let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).await.ok();
        Ok(RedirectListener { v4, v6, port })
    }

    fn port(&self) -> u16 {
        self.port
    }

    /// Serves the loopback port until the redirect of `request` arrives. Other requests (a
    /// favicon, a preconnect) get a 404, and redirects with another `state` (an old tab of an
    /// earlier login) a 400; both are ignored.
    async fn wait_for_code(
        self,
        request: &AuthorizationRequest,
        timeout: Duration,
    ) -> Result<String, LoginError> {
        let (sender, mut receiver) = mpsc::channel(1);
        let state: Arc<str> = request.state.as_str().into();
        // Dropping the set at the end aborts the connections that are still open.
        let mut connections = JoinSet::new();
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);

        loop {
            tokio::select! {
                accepted = self.v4.accept() => {
                    if let Ok((stream, _)) = accepted {
                        connections.spawn(serve(stream, Arc::clone(&state), sender.clone()));
                    }
                }
                accepted = accept(self.v6.as_ref()) => {
                    if let Ok((stream, _)) = accepted {
                        connections.spawn(serve(stream, Arc::clone(&state), sender.clone()));
                    }
                }
                Some(result) = receiver.recv() => return result,
                () = &mut deadline => {
                    return Err(LoginError::Timeout {
                        port: self.port,
                        minutes: timeout.as_secs() / 60,
                    });
                }
            }
        }
    }
}

async fn accept(listener: Option<&TcpListener>) -> io::Result<(TcpStream, std::net::SocketAddr)> {
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

/// Answers one connection to the loopback listener and reports the redirect, if it is one.
async fn serve(
    mut stream: TcpStream,
    state: Arc<str>,
    sender: mpsc::Sender<Result<String, LoginError>>,
) {
    let Ok(Some(target)) = tokio::time::timeout(CONNECTION_TIMEOUT, read_target(&mut stream)).await
    else {
        return;
    };
    let result = Url::parse("http://localhost")
        .and_then(|base| base.join(&target))
        .ok()
        .and_then(|url| code_from_query(&url, &state));

    let Some(result) = result else {
        respond(&mut stream, StatusCode::NOT_FOUND, "Not found.").await;
        return;
    };
    match &result {
        Ok(_) => {
            respond(
                &mut stream,
                StatusCode::OK,
                "gdrive received the authorization. You can close this window and go back to \
                 the terminal.",
            )
            .await;
        }
        Err(LoginError::StateMismatch) => {
            // Not this login's answer: it must not end the login that is waiting.
            respond(
                &mut stream,
                StatusCode::BAD_REQUEST,
                "This page belongs to another gdrive login and was ignored. Open the URL that \
                 gdrive printed last; gdrive is still waiting for it.",
            )
            .await;
            return;
        }
        Err(error) => {
            let message = format!(
                "gdrive could not use this authorization: {error}. Go back to the terminal."
            );
            respond(&mut stream, StatusCode::BAD_REQUEST, &message).await;
        }
    }
    // The page is written before the result is reported, so the browser gets all of it.
    let _ = sender.send(result).await;
}

/// The request target of an HTTP/1.x request, after reading the whole request head.
async fn read_target(stream: &mut TcpStream) -> Option<String> {
    let mut head = Vec::new();
    let mut chunk = [0; 2048];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        if head.len() > MAX_REQUEST_HEAD {
            return None;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        head.extend_from_slice(&chunk[..read]);
    }

    let head = std::str::from_utf8(&head).ok()?;
    let mut request_line = head.lines().next()?.split(' ');
    let method = request_line.next()?;
    let target = request_line.next()?;
    (method == "GET" && target.starts_with('/')).then(|| target.to_owned())
}

async fn respond(stream: &mut TcpStream, status: StatusCode, message: &str) {
    let body = format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>gdrive</title></head>\
         <body><p>{}</p></body></html>",
        escape_html(message)
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn escape_html(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            c => escaped.push(c),
        }
    }
    escaped
}

/// Exchanges the authorization code at the token endpoint, with the PKCE verifier.
async fn exchange_code<C>(
    client: &Client<C, String>,
    token_uri: &str,
    secret: &Secret,
    request: &AuthorizationRequest,
    code: &str,
) -> Result<Authorization, LoginError>
where
    C: Connect + Clone + Send + Sync + 'static,
{
    let body = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("code", code)
        .append_pair("client_id", &secret.client_id)
        .append_pair("client_secret", &secret.client_secret)
        .append_pair("redirect_uri", &request.redirect_uri)
        .append_pair("grant_type", "authorization_code")
        .append_pair("code_verifier", &request.code_verifier)
        .finish();
    let http_request = Request::post(token_uri)
        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(body)
        .map_err(|_| LoginError::InvalidEndpoint)?;

    let response = tokio::time::timeout(TOKEN_REQUEST_TIMEOUT, client.request(http_request))
        .await
        .map_err(|_| LoginError::TokenRequestTimeout)?
        .map_err(LoginError::TokenRequest)?;
    let status = response.status();
    let body = tokio::time::timeout(TOKEN_REQUEST_TIMEOUT, response.into_body().collect())
        .await
        .map_err(|_| LoginError::TokenRequestTimeout)?
        .map_err(LoginError::TokenResponse)?
        .to_bytes();

    token_from_response(status, &body, request.audit_log)
}

/// The authorization in the answer of the token endpoint, checked against what was asked for.
fn token_from_response(
    status: StatusCode,
    body: &[u8],
    audit_log: AuditLog,
) -> Result<Authorization, LoginError> {
    #[derive(Deserialize)]
    struct TokenResponse {
        access_token: Option<String>,
        refresh_token: Option<String>,
        expires_in: Option<i64>,
        id_token: Option<String>,
        scope: Option<String>,
        error: Option<String>,
        error_description: Option<String>,
    }

    // Do not put the body into the error: on success it holds the tokens.
    let response: TokenResponse =
        serde_json::from_slice(body).map_err(|_| LoginError::InvalidTokenResponse(status))?;

    if response.error.is_some() || !status.is_success() {
        return Err(LoginError::TokenEndpoint {
            status,
            error: response.error.unwrap_or_else(|| String::from("unknown")),
            description: response.error_description,
        });
    }
    // Without a `scope` field, what was asked for is granted.
    let granted = |wanted: &str| {
        response
            .scope
            .as_deref()
            .is_none_or(|scope| scope.split(' ').any(|scope| scope == wanted))
    };
    if !granted(DRIVE_SCOPES[0]) {
        return Err(LoginError::DriveScopeNotGranted);
    }
    let audit_log_granted = audit_log != AuditLog::No && granted(AUDIT_LOG_SCOPE);
    if audit_log == AuditLog::Required && !audit_log_granted {
        return Err(LoginError::AuditLogNotGranted);
    }
    let access_token = response.access_token.ok_or(LoginError::NoAccessToken)?;

    Ok(Authorization {
        token: TokenInfo {
            access_token: Some(access_token),
            refresh_token: response.refresh_token,
            expires_at: response
                .expires_in
                .map(|seconds| time::OffsetDateTime::now_utc() + time::Duration::seconds(seconds)),
            id_token: response.id_token,
        },
        audit_log: audit_log_granted,
    })
}

#[derive(Debug)]
pub(crate) enum LoginError {
    Random,
    InvalidEndpoint,
    HttpClient(io::Error),
    PortInUse(u16),
    Listen {
        port: u16,
        source: io::Error,
    },
    ManualNeedsPort,
    Timeout {
        port: u16,
        minutes: u64,
    },
    ReadInput(io::Error),
    InputClosed,
    NotARedirect,
    StateMismatch,
    Denied {
        error: String,
        description: Option<String>,
    },
    TokenRequest(hyper_util::client::legacy::Error),
    TokenRequestTimeout,
    TokenResponse(hyper::Error),
    InvalidTokenResponse(StatusCode),
    TokenEndpoint {
        status: StatusCode,
        error: String,
        description: Option<String>,
    },
    NoAccessToken,
    DriveScopeNotGranted,
    /// `--audit-log` was given and the user unticked the audit log on the consent screen.
    AuditLogNotGranted,
}

impl Display for LoginError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            LoginError::Random => f.write_str("unable to get random bytes from the system"),
            LoginError::InvalidEndpoint => f.write_str("invalid OAuth endpoint URL"),
            LoginError::HttpClient(_) => f.write_str("unable to create the HTTP client"),
            LoginError::PortInUse(port) => write!(
                f,
                "port {port} is already in use, so gdrive cannot receive the authorization \
                 redirect on http://localhost:{port} (an earlier 'gdrive account add' may still \
                 be waiting there). Choose another port with --port N, or use --manual to paste \
                 the redirect URL instead"
            ),
            LoginError::Listen { port, .. } => write!(
                f,
                "unable to listen on port {port} for the authorization redirect; choose another \
                 port with --port N, or use --manual"
            ),
            LoginError::ManualNeedsPort => f.write_str(
                "--manual needs a port number for the redirect URI; --port 0 only works without \
                 --manual",
            ),
            LoginError::Timeout { port, minutes } => write!(
                f,
                "no authorization redirect arrived on http://localhost:{port} within {minutes} \
                 minutes; run the command again, with --manual if the browser runs on another \
                 machine"
            ),
            LoginError::ReadInput(_) => f.write_str("unable to read the redirect URL"),
            LoginError::InputClosed => f.write_str("no redirect URL was entered (end of input)"),
            LoginError::NotARedirect => {
                f.write_str("the text is not the redirect URL; it must contain code= and state=")
            }
            LoginError::StateMismatch => {
                f.write_str("the redirect belongs to another login (its state does not match)")
            }
            LoginError::Denied { error, description } => {
                write!(f, "the authorization was not granted ({error}")?;
                if let Some(description) = description {
                    write!(f, ": {description}")?;
                }
                f.write_str(")")
            }
            LoginError::TokenRequest(_) => {
                f.write_str("unable to send the authorization code to the token endpoint")
            }
            LoginError::TokenRequestTimeout => write!(
                f,
                "no answer from the token endpoint within {} seconds",
                TOKEN_REQUEST_TIMEOUT.as_secs()
            ),
            LoginError::TokenResponse(_) => {
                f.write_str("unable to read the answer of the token endpoint")
            }
            LoginError::InvalidTokenResponse(status) => write!(
                f,
                "the token endpoint answered with an unexpected response (HTTP {status})"
            ),
            LoginError::TokenEndpoint {
                status,
                error,
                description,
            } => {
                write!(
                    f,
                    "the token endpoint refused the authorization code (HTTP {status}, {error}"
                )?;
                if let Some(description) = description {
                    write!(f, ": {description}")?;
                }
                f.write_str(")")
            }
            LoginError::NoAccessToken => {
                f.write_str("the token endpoint answered without an access token")
            }
            LoginError::DriveScopeNotGranted => f.write_str(
                "the authorization does not include access to Google Drive; run the command \
                 again and allow gdrive to see, edit, create and delete your Google Drive files",
            ),
            LoginError::AuditLogNotGranted => f.write_str(
                "the authorization does not include read access to the audit log of the Google \
                 Workspace organization, so nothing was saved; run the command again and leave \
                 the box for the audit log ticked on the consent screen (the Google account must \
                 be an administrator of the organization)",
            ),
        }
    }
}

impl error::Error for LoginError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        match self {
            LoginError::HttpClient(source)
            | LoginError::Listen { source, .. }
            | LoginError::ReadInput(source) => Some(source),
            LoginError::TokenRequest(source) => Some(source),
            LoginError::TokenResponse(source) => Some(source),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, net::SocketAddr};

    use super::*;
    use crate::auth::{
        store::TokenFile,
        stored::StoredCredentials,
        test_support::{redirected_client, MockServer},
    };

    fn secret() -> Secret {
        Secret {
            client_id: String::from("client-id.apps.googleusercontent.com"),
            client_secret: String::from("client-secret"),
        }
    }

    fn request(redirect_uri: &str) -> AuthorizationRequest {
        request_for(redirect_uri, AuditLog::No)
    }

    fn request_for(redirect_uri: &str, audit_log: AuditLog) -> AuthorizationRequest {
        AuthorizationRequest::new(
            GOOGLE_AUTH_URI,
            &secret().client_id,
            redirect_uri,
            Some("me@example.com"),
            audit_log,
        )
        .unwrap()
    }

    fn query(url: &str) -> HashMap<String, String> {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    fn http_client() -> Client<hyper_util::client::legacy::connect::HttpConnector, String> {
        Client::builder(hyper_util::rt::TokioExecutor::new()).build_http()
    }

    #[test]
    fn authorization_url_has_pkce_state_offline_access_and_consent() {
        let request = request("http://localhost:8085");
        assert!(request
            .url()
            .starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));

        let params = query(request.url());
        assert_eq!(params["client_id"], "client-id.apps.googleusercontent.com");
        assert_eq!(params["redirect_uri"], "http://localhost:8085");
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["scope"], DRIVE_SCOPES.join(" "));
        assert_eq!(params["access_type"], "offline");
        assert_eq!(params["prompt"], "consent");
        assert_eq!(params["login_hint"], "me@example.com");
        assert_eq!(params["code_challenge_method"], "S256");
        assert_eq!(params["state"], request.state);
        assert_eq!(request.state.len(), 43);
        assert_eq!(request.code_verifier.len(), 43);
        let challenge = URL_SAFE_NO_PAD.encode(ring::digest::digest(
            &ring::digest::SHA256,
            request.code_verifier.as_bytes(),
        ));
        assert_eq!(params["code_challenge"], challenge);
        assert!(!request.url().contains(&request.code_verifier));
        assert!(!request.url().contains("urn:ietf:wg:oauth:2.0:oob"));

        let other = self::request("http://localhost:8085");
        assert_ne!(other.state, request.state);
        assert_ne!(other.code_verifier, request.code_verifier);
    }

    fn code_challenge(request: &AuthorizationRequest) -> String {
        URL_SAFE_NO_PAD.encode(ring::digest::digest(
            &ring::digest::SHA256,
            request.code_verifier.as_bytes(),
        ))
    }

    #[test]
    fn authorization_url_without_the_audit_log_is_that_of_gdrive_4_0() {
        let request = request("http://localhost:8085");
        let expected = format!(
            "https://accounts.google.com/o/oauth2/v2/auth?\
             client_id=client-id.apps.googleusercontent.com\
             &redirect_uri=http%3A%2F%2Flocalhost%3A8085&response_type=code\
             &scope=https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive\
             +https%3A%2F%2Fwww.googleapis.com%2Fauth%2Fdrive.metadata.readonly\
             &access_type=offline&prompt=consent&state={}&code_challenge={}\
             &code_challenge_method=S256&login_hint=me%40example.com",
            request.state,
            code_challenge(&request)
        );
        assert_eq!(request.url(), expected);
        assert!(!request.url().contains("admin.reports"));
        assert_eq!(
            Options::new(Mode::Browser, 8085, None).audit_log,
            AuditLog::No
        );
    }

    #[test]
    fn authorization_url_asks_for_the_audit_log_too_with_the_flag() {
        let request = request_for("http://localhost:8085", AuditLog::Required);
        let mut params = query(request.url());
        assert_eq!(
            params["scope"],
            "https://www.googleapis.com/auth/drive \
             https://www.googleapis.com/auth/drive.metadata.readonly \
             https://www.googleapis.com/auth/admin.reports.audit.readonly"
        );
        assert_eq!(params["code_challenge"], code_challenge(&request));
        assert_eq!(params["state"], request.state);

        // `account reauth` asks for it again in the same way.
        let again = request_for("http://localhost:8085", AuditLog::Optional);
        assert_eq!(query(again.url())["scope"], params["scope"]);

        // Everything else is as without the flag.
        let plain = self::request("http://localhost:8085");
        let mut plain_params = query(plain.url());
        for params in [&mut params, &mut plain_params] {
            for name in ["scope", "state", "code_challenge"] {
                params.remove(name);
            }
        }
        assert_eq!(params, plain_params);
    }

    #[test]
    fn pkce_challenge_matches_rfc_7636_example() {
        // RFC 7636 appendix B.
        let challenge = URL_SAFE_NO_PAD.encode(ring::digest::digest(
            &ring::digest::SHA256,
            b"dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
        ));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn pasted_redirect_urls_are_accepted_in_several_forms() {
        let request = request("http://localhost:8085");
        let state = &request.state;
        for pasted in [
            format!("http://localhost:8085/?state={state}&code=4/0Ab-c%2Fd&scope=x"),
            format!("  'http://localhost:8085/?code=4/0Ab-c%2Fd&state={state}'\n"),
            format!("localhost:8085/?code=4/0Ab-c%2Fd&state={state}"),
            format!("?code=4/0Ab-c%2Fd&state={state}"),
            format!("code=4/0Ab-c%2Fd&state={state}"),
        ] {
            assert_eq!(
                request.code_from_pasted(&pasted).unwrap(),
                "4/0Ab-c/d",
                "{pasted}"
            );
        }
    }

    #[test]
    fn pasted_redirects_are_checked() {
        let request = request("http://localhost:8085");

        let wrong_state = "http://localhost:8085/?code=abc&state=forged";
        assert!(matches!(
            request.code_from_pasted(wrong_state),
            Err(LoginError::StateMismatch)
        ));
        let no_state = "http://localhost:8085/?code=abc";
        assert!(matches!(
            request.code_from_pasted(no_state),
            Err(LoginError::StateMismatch)
        ));

        let denied = format!(
            "http://localhost:8085/?error=access_denied&state={}",
            request.state
        );
        let Err(LoginError::Denied { error, .. }) = request.code_from_pasted(&denied) else {
            panic!("expected Denied");
        };
        assert_eq!(error, "access_denied");

        // A refusal is only believed with this login's state.
        for denied in [
            "http://localhost:8085/?error=access_denied&state=forged",
            "http://localhost:8085/?error=access_denied",
        ] {
            assert!(
                matches!(
                    request.code_from_pasted(denied),
                    Err(LoginError::StateMismatch)
                ),
                "{denied}"
            );
        }

        // The authorization URL itself, or anything else, is not a redirect.
        for pasted in [request.url(), "hello", "https://example.com/"] {
            assert!(
                matches!(
                    request.code_from_pasted(pasted),
                    Err(LoginError::NotARedirect)
                ),
                "{pasted}"
            );
        }
    }

    #[tokio::test]
    async fn manual_input_is_read_until_a_redirect_is_pasted() {
        let request = request("http://localhost:8085");
        let input = format!(
            "\nnot a url\nhttp://localhost:8085/?code=old&state=earlier-login\n\
             http://localhost:8085/?code=the-code&state={}\n",
            request.state
        );
        let mut reader = tokio::io::BufReader::new(input.as_bytes());
        assert_eq!(read_code(&request, &mut reader).await.unwrap(), "the-code");

        let mut empty = tokio::io::BufReader::new(&b"garbage\n"[..]);
        assert!(matches!(
            read_code(&request, &mut empty).await,
            Err(LoginError::InputClosed)
        ));
    }

    /// The token endpoint answer of a successful exchange.
    const TOKEN_RESPONSE: &str = r#"{"access_token":"ya29.access","expires_in":3599,"refresh_token":"1//refresh","scope":"https://www.googleapis.com/auth/drive https://www.googleapis.com/auth/drive.metadata.readonly","token_type":"Bearer"}"#;

    #[tokio::test]
    async fn manual_login_exchanges_the_code_with_the_verifier_and_stores_the_token() {
        let server = MockServer::start(vec![(200, String::from(TOKEN_RESPONSE))]).await;
        let options = Options::new(Mode::Manual, 8085, None)
            .with_endpoints(String::from(GOOGLE_AUTH_URI), server.url("/token"));
        let request = AuthorizationRequest::new(
            &options.endpoints.auth_uri,
            &secret().client_id,
            "http://localhost:8085",
            None,
            options.audit_log,
        )
        .unwrap();

        // What the user pastes after approving in a browser elsewhere.
        let pasted = format!(
            "http://localhost:8085/?code=4%2F0Abc&state={}",
            request.state
        );
        let code = request.code_from_pasted(&pasted).unwrap();
        let authorization = exchange_code(
            &http_client(),
            &options.endpoints.token_uri,
            &secret(),
            &request,
            &code,
        )
        .await
        .unwrap();
        assert!(!authorization.audit_log);
        assert_eq!(authorization.scopes(), DRIVE_SCOPES);
        let token = authorization.token;

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].target, "/token");
        assert_eq!(
            requests[0].form("grant_type").as_deref(),
            Some("authorization_code")
        );
        assert_eq!(requests[0].form("code").as_deref(), Some("4/0Abc"));
        assert_eq!(
            requests[0].form("redirect_uri").as_deref(),
            Some("http://localhost:8085")
        );
        assert_eq!(
            requests[0].form("client_id").as_deref(),
            Some("client-id.apps.googleusercontent.com")
        );
        assert_eq!(
            requests[0].form("client_secret").as_deref(),
            Some("client-secret")
        );
        let verifier = requests[0].form("code_verifier").unwrap();
        let challenge = URL_SAFE_NO_PAD.encode(ring::digest::digest(
            &ring::digest::SHA256,
            verifier.as_bytes(),
        ));
        assert_eq!(query(request.url())["code_challenge"], challenge);

        assert_eq!(token.access_token.as_deref(), Some("ya29.access"));
        assert_eq!(token.refresh_token.as_deref(), Some("1//refresh"));
        assert!(!token.is_expired());

        // Stored like `account add` does, the token is used by the refresh-only path as is.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tokens.json");
        TokenFile::after_login(None, &DRIVE_SCOPES, token)
            .save(&path)
            .await
            .unwrap();
        let json = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(!TokenFile::from_json(json.as_bytes())
            .unwrap()
            .audit_log_granted());
        assert!(json.starts_with(
            r#"[{"scopes":["https://www.googleapis.com/auth/drive","https://www.googleapis.com/auth/drive.metadata.readonly"],"token":{"access_token":"ya29.access","refresh_token":"1//refresh","expires_at":["#
        ));
        let credentials =
            StoredCredentials::load(redirected_client(server.addr()), "me", &secret(), &path)
                .await
                .unwrap();
        let access = credentials.access_token(&DRIVE_SCOPES[1..]).await.unwrap();
        assert_eq!(access.token(), Some("ya29.access"));
        assert_eq!(server.requests().len(), 1);
    }

    #[tokio::test]
    async fn token_endpoint_errors_are_reported_without_the_response_body() {
        let cases = [
            (
                400,
                r#"{"error":"invalid_grant","error_description":"Bad Request"}"#,
                "the token endpoint refused the authorization code (HTTP 400 Bad Request, \
                 invalid_grant: Bad Request)",
            ),
            (
                502,
                "<html>gateway</html>",
                "the token endpoint answered with an unexpected response (HTTP 502 Bad Gateway)",
            ),
            (
                200,
                r#"{"access_token":"secret-token","scope":"https://www.googleapis.com/auth/drive.metadata.readonly"}"#,
                "the authorization does not include access to Google Drive",
            ),
        ];
        for (status, body, expected) in cases {
            let server = MockServer::start(vec![(status, String::from(body))]).await;
            let request = request("http://localhost:8085");
            let error = exchange_code(
                &http_client(),
                &server.url("/token"),
                &secret(),
                &request,
                "code",
            )
            .await
            .unwrap_err();
            let message = error.to_string();
            assert!(message.starts_with(expected), "{message}");
            assert!(!message.contains("secret-token"));
        }
    }

    /// The answer of the token endpoint to a login, with `scope` as its scope field.
    fn token_response(scope: Option<&str>) -> String {
        let mut response = serde_json::json!({
            "access_token": "ya29.secret-access",
            "expires_in": 3599,
            "refresh_token": "1//secret-refresh",
            "token_type": "Bearer",
        });
        if let Some(scope) = scope {
            response["scope"] = scope.into();
        }
        response.to_string()
    }

    const DRIVE_GRANTED: &str = "https://www.googleapis.com/auth/drive \
                                 https://www.googleapis.com/auth/drive.metadata.readonly";
    const ALL_GRANTED: &str = "https://www.googleapis.com/auth/drive \
                               https://www.googleapis.com/auth/drive.metadata.readonly \
                               https://www.googleapis.com/auth/admin.reports.audit.readonly";

    #[test]
    fn the_token_response_says_whether_the_audit_log_was_granted() {
        // (asked, scope field of the answer, audit log granted)
        let granted = [
            (AuditLog::Required, Some(ALL_GRANTED), true),
            // Google lists the granted scopes in any order.
            (
                AuditLog::Required,
                Some(
                    "https://www.googleapis.com/auth/admin.reports.audit.readonly \
                     https://www.googleapis.com/auth/drive",
                ),
                true,
            ),
            // Without a scope field, what was asked for is granted.
            (AuditLog::Required, None, true),
            // Asked for again by `account reauth`: granted or not, the login succeeds.
            (AuditLog::Optional, Some(ALL_GRANTED), true),
            (AuditLog::Optional, None, true),
            (AuditLog::Optional, Some(DRIVE_GRANTED), false),
            (AuditLog::No, None, false),
            (AuditLog::No, Some(DRIVE_GRANTED), false),
            // Not asked for, so not recorded.
            (AuditLog::No, Some(ALL_GRANTED), false),
        ];
        for (asked, scope, expected) in granted {
            let body = token_response(scope);
            let authorization = token_from_response(StatusCode::OK, body.as_bytes(), asked)
                .unwrap_or_else(|error| panic!("{asked:?} {scope:?}: {error}"));
            assert_eq!(authorization.audit_log, expected, "{asked:?} {scope:?}");
            let scopes = if expected {
                &DRIVE_AND_AUDIT_LOG_SCOPES[..]
            } else {
                &DRIVE_SCOPES[..]
            };
            assert_eq!(authorization.scopes(), scopes);
            assert_eq!(
                authorization.token.refresh_token.as_deref(),
                Some("1//secret-refresh")
            );
        }
    }

    #[test]
    fn a_required_audit_log_that_was_not_granted_fails_the_login() {
        let body = token_response(Some(DRIVE_GRANTED));
        let error =
            token_from_response(StatusCode::OK, body.as_bytes(), AuditLog::Required).unwrap_err();
        assert!(matches!(error, LoginError::AuditLogNotGranted), "{error:?}");
        let message = crate::error_message(&error);
        assert_eq!(
            message,
            "Error: the authorization does not include read access to the audit log of the \
             Google Workspace organization, so nothing was saved; run the command again and \
             leave the box for the audit log ticked on the consent screen (the Google account \
             must be an administrator of the organization)"
        );
        assert!(!message.contains("secret"), "{message}");

        // The Drive scope is still checked first.
        let body = token_response(Some(AUDIT_LOG_SCOPE));
        let error =
            token_from_response(StatusCode::OK, body.as_bytes(), AuditLog::Required).unwrap_err();
        assert!(
            matches!(error, LoginError::DriveScopeNotGranted),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn an_audit_log_login_checks_the_answer_of_the_token_endpoint() {
        for (scope, granted) in [(ALL_GRANTED, true), (DRIVE_GRANTED, false)] {
            let server = MockServer::start(vec![(200, token_response(Some(scope)))]).await;
            let request = request_for("http://localhost:8085", AuditLog::Required);
            let result = exchange_code(
                &http_client(),
                &server.url("/token"),
                &secret(),
                &request,
                "code",
            )
            .await;
            assert_eq!(server.requests().len(), 1);
            match result {
                Ok(authorization) => {
                    assert!(granted, "{scope}");
                    assert!(authorization.audit_log);
                    // Stored like `account add` does, the entry records the grant.
                    let tokens =
                        TokenFile::after_login(None, authorization.scopes(), authorization.token);
                    assert!(tokens.audit_log_granted());
                    let json = tokens.to_json().unwrap();
                    assert!(
                        json.starts_with(
                            r#"[{"scopes":["https://www.googleapis.com/auth/drive","https://www.googleapis.com/auth/drive.metadata.readonly","https://www.googleapis.com/auth/admin.reports.audit.readonly"],"token":{"access_token":"ya29.secret-access","refresh_token":"1//secret-refresh","#
                        ),
                        "the stored entry"
                    );
                }
                Err(error) => {
                    assert!(!granted, "{scope}: {error}");
                    assert!(matches!(error, LoginError::AuditLogNotGranted));
                    assert!(!error.to_string().contains("secret"));
                }
            }
        }
    }

    /// Sends a GET to the listener like a browser and returns the response.
    async fn browse(addr: SocketAddr, target: &str) -> String {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\nAccept: */*\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[tokio::test]
    async fn browser_mode_receives_the_redirect_and_ignores_other_requests() {
        let listener = RedirectListener::bind(0).await.unwrap();
        let port = listener.port();
        assert_ne!(port, 0);
        let request = request(&format!("http://localhost:{port}"));
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let redirect = format!("/?state={}&code=4%2Fcode&scope=x", request.state);

        let browser = async {
            // A connection that sends nothing must not block the listener.
            let _idle = TcpStream::connect(addr).await.unwrap();
            let favicon = browse(addr, "/favicon.ico").await;
            assert!(favicon.starts_with("HTTP/1.1 404"), "{favicon}");
            let page = browse(addr, &redirect).await;
            assert!(page.starts_with("HTTP/1.1 200"), "{page}");
            assert!(page.contains("You can close this window"));
            assert!(!page.contains("4/code"));
        };
        let (code, ()) = tokio::join!(
            listener.wait_for_code(&request, Duration::from_secs(30)),
            browser
        );
        assert_eq!(code.unwrap(), "4/code");

        // The port is free again.
        drop(RedirectListener::bind(port).await.unwrap());
    }

    #[tokio::test]
    async fn browser_mode_ignores_callbacks_of_another_login() {
        let listener = RedirectListener::bind(0).await.unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, listener.port()));
        let request = request(&format!("http://localhost:{}", listener.port()));
        let good = format!("/?code=4%2Fgood&state={}", request.state);

        let browser = async {
            // An old tab of an earlier login is reloaded: its code, then its refusal.
            for stale in [
                "/?code=old&state=earlier-login",
                "/?error=access_denied&state=earlier-login",
                "/?error=access_denied",
            ] {
                let page = browse(addr, stale).await;
                assert!(page.starts_with("HTTP/1.1 400"), "{stale}: {page}");
                assert!(page.contains("still waiting"), "{stale}: {page}");
            }
            let page = browse(addr, &good).await;
            assert!(page.starts_with("HTTP/1.1 200"), "{page}");
        };
        let (code, ()) = tokio::join!(
            listener.wait_for_code(&request, Duration::from_secs(30)),
            browser
        );
        assert_eq!(code.unwrap(), "4/good");
    }

    #[tokio::test]
    async fn browser_mode_reports_a_refusal_of_this_login() {
        let listener = RedirectListener::bind(0).await.unwrap();
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, listener.port()));
        let request = request(&format!("http://localhost:{}", listener.port()));
        let denied = format!("/?error=access_denied&state={}", request.state);

        let (result, page) = tokio::join!(
            listener.wait_for_code(&request, Duration::from_secs(30)),
            browse(addr, &denied)
        );
        assert!(page.starts_with("HTTP/1.1 400"), "{page}");
        let Err(LoginError::Denied { error, .. }) = result else {
            panic!("expected Denied, got {result:?}");
        };
        assert_eq!(error, "access_denied");
    }

    #[tokio::test]
    async fn busy_port_is_reported_with_the_way_out() {
        let taken = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = taken.local_addr().unwrap().port();

        let Err(error) = RedirectListener::bind(port).await else {
            panic!("bound a busy port");
        };
        assert!(matches!(error, LoginError::PortInUse(p) if p == port));
        let message = error.to_string();
        assert!(message.contains(&format!("port {port} is already in use")));
        assert!(message.contains("--port N"));
        assert!(message.contains("--manual"));
    }

    #[tokio::test]
    async fn browser_mode_gives_up_after_the_timeout() {
        let listener = RedirectListener::bind(0).await.unwrap();
        let request = request(&format!("http://localhost:{}", listener.port()));
        let result = listener
            .wait_for_code(&request, Duration::from_millis(50))
            .await;
        assert!(matches!(result, Err(LoginError::Timeout { .. })));
    }

    #[tokio::test]
    async fn manual_mode_needs_a_real_port() {
        let options = Options::new(Mode::Manual, 0, None);
        let error = login(&secret(), &options).await.unwrap_err();
        assert!(matches!(error, LoginError::ManualNeedsPort));
    }

    #[test]
    fn html_is_escaped() {
        assert_eq!(
            escape_html("<script>'&\"</script>"),
            "&lt;script&gt;&#39;&amp;&quot;&lt;/script&gt;"
        );
    }
}
