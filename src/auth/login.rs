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

use super::DRIVE_SCOPES;
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

#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) mode: Mode,
    /// Port of the redirect URI. In browser mode, 0 picks a free port.
    pub(crate) port: u16,
    /// Preselects the Google account on the consent screen.
    pub(crate) login_hint: Option<String>,
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
            endpoints: Endpoints::default(),
        }
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

/// Asks the user to authorize gdrive and returns the token Google issued.
pub(crate) async fn login(secret: &Secret, options: &Options) -> Result<TokenInfo, LoginError> {
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
    println!(
        "Google then redirects the browser to http://localhost:{port}/?code=... and the page \
         fails to load; that is expected. Copy the full address from the browser's address bar \
         and paste it here."
    );
}

/// One authorization attempt: the URL to open and the secrets the redirect is checked with.
pub(crate) struct AuthorizationRequest {
    url: String,
    redirect_uri: String,
    state: String,
    code_verifier: String,
}

impl AuthorizationRequest {
    fn new(
        auth_uri: &str,
        client_id: &str,
        redirect_uri: &str,
        login_hint: Option<&str>,
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
                .append_pair("scope", &DRIVE_SCOPES.join(" "))
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

    if let Some(error) = error {
        return Some(Err(LoginError::Denied {
            error,
            description: error_description,
        }));
    }
    let code = code?;
    if state.as_deref() != Some(expected_state) {
        return Some(Err(LoginError::StateMismatch));
    }
    Some(Ok(code))
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
    /// favicon, a preconnect) get a 404 and are ignored.
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
) -> Result<TokenInfo, LoginError>
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

    token_from_response(status, &body)
}

fn token_from_response(status: StatusCode, body: &[u8]) -> Result<TokenInfo, LoginError> {
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
    if let Some(scope) = &response.scope {
        if !scope.split(' ').any(|scope| scope == DRIVE_SCOPES[0]) {
            return Err(LoginError::DriveScopeNotGranted);
        }
    }
    let access_token = response.access_token.ok_or(LoginError::NoAccessToken)?;

    Ok(TokenInfo {
        access_token: Some(access_token),
        refresh_token: response.refresh_token,
        expires_at: response
            .expires_in
            .map(|seconds| time::OffsetDateTime::now_utc() + time::Duration::seconds(seconds)),
        id_token: response.id_token,
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
            LoginError::StateMismatch => f.write_str(
                "the redirect does not belong to this login (its state does not match); run the \
                 command again and use the URL it prints",
            ),
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
        AuthorizationRequest::new(
            GOOGLE_AUTH_URI,
            &secret().client_id,
            redirect_uri,
            Some("me@example.com"),
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
            "\nnot a url\nhttp://localhost:8085/?code=the-code&state={}\n",
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
        )
        .unwrap();

        // What the user pastes after approving in a browser elsewhere.
        let pasted = format!(
            "http://localhost:8085/?code=4%2F0Abc&state={}",
            request.state
        );
        let code = request.code_from_pasted(&pasted).unwrap();
        let token = exchange_code(
            &http_client(),
            &options.endpoints.token_uri,
            &secret(),
            &request,
            &code,
        )
        .await
        .unwrap();

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
    async fn browser_mode_rejects_a_foreign_state_and_reports_denials() {
        for (target, expected_status) in [
            ("/?code=abc&state=forged", "HTTP/1.1 400"),
            ("/?error=access_denied&state=x", "HTTP/1.1 400"),
        ] {
            let listener = RedirectListener::bind(0).await.unwrap();
            let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, listener.port()));
            let request = request(&format!("http://localhost:{}", listener.port()));

            let (result, page) = tokio::join!(
                listener.wait_for_code(&request, Duration::from_secs(30)),
                browse(addr, target)
            );
            assert!(page.starts_with(expected_status), "{page}");
            match result {
                Err(LoginError::StateMismatch) => assert!(target.contains("forged")),
                Err(LoginError::Denied { error, .. }) => assert_eq!(error, "access_denied"),
                other => panic!("unexpected result {other:?}"),
            }
        }
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
