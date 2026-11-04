//! In-process HTTP mocks for the OAuth tests. Nothing here talks to Google.

use std::{
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use hyper::Uri;
use hyper_util::rt::TokioIo;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

/// A request the mock server received.
#[derive(Debug, Clone)]
pub(crate) struct RecordedRequest {
    pub(crate) method: String,
    pub(crate) target: String,
    pub(crate) body: String,
}

impl RecordedRequest {
    /// The value of a field of an `application/x-www-form-urlencoded` body.
    pub(crate) fn form(&self, name: &str) -> Option<String> {
        url::form_urlencoded::parse(self.body.as_bytes())
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    }
}

/// An HTTP/1.1 server that answers every request with the next canned `(status, JSON body)`
/// response (the last one repeats) and records what it received.
pub(crate) struct MockServer {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    task: JoinHandle<()>,
}

impl MockServer {
    pub(crate) async fn start(responses: Vec<(u16, String)>) -> MockServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);

        let task = tokio::spawn(async move {
            let mut count = 0;
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let Some(request) = read_request(&mut stream).await else {
                    continue;
                };
                recorded.lock().unwrap().push(request);
                let (status, body) = responses[count.min(responses.len() - 1)].clone();
                count += 1;
                let response = format!(
                    "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        MockServer {
            addr,
            requests,
            task,
        }
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub(crate) fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.addr)
    }

    pub(crate) fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<RecordedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0; 4096];
    let head_end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
    };

    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let target = request_line.next()?.to_owned();
    let content_length = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse::<usize>().ok())
        .unwrap_or(0);

    while buffer.len() < head_end + content_length {
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body = String::from_utf8_lossy(&buffer[head_end..]).into_owned();

    Some(RecordedRequest {
        method,
        target,
        body,
    })
}

/// A connector that sends every request, whatever its host and scheme, over plain TCP to one
/// address. Used for code that has a Google URL built in (yup-oauth2's authorized-user flow).
#[derive(Clone)]
pub(crate) struct RedirectConnector(pub(crate) SocketAddr);

impl tower_service::Service<Uri> for RedirectConnector {
    type Response = TokioIo<TcpStream>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        let addr = self.0;
        Box::pin(async move { TcpStream::connect(addr).await.map(TokioIo::new) })
    }
}

/// An HTTP client whose every request goes to `addr`.
pub(crate) fn redirected_client(
    addr: SocketAddr,
) -> hyper_util::client::legacy::Client<RedirectConnector, String> {
    hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .build(RedirectConnector(addr))
}

/// A throwaway 2048-bit RSA key, generated for these tests only: it belongs to no service account
/// and works nowhere. Service account key files carry the key as a PKCS#8 PEM string.
pub(crate) const TEST_PRIVATE_KEY: &str = "\
    -----BEGIN PRIVATE KEY-----\n\
    MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCstWFkjXBpOnbq\n\
    VPAkAFQomzrwA1MVeKF9Xbr2nFosuqtM97qRyW+QYEgr9Ku7wN7f6zTu9UElpwls\n\
    dBx7MtKxmMQj2ox+KJL2LXKltW/yc9fIOJVj/YCJsZC6ucEQjo9hzTAGLPNWcWZi\n\
    wnX/y51aS8gcjlVMsOh6hsi4PBew/C+m3mUjKDrSlHuSHMH5XaQQFcKnBLyYSDEH\n\
    NpqYbc9HvFbldAReI/u6HCTh6TVjLroMhjHRFmcI84kesT5uE7iFzgz9516q7WXk\n\
    H1vKVFv5W3AQetxePdgSo6FtJP3dKrEQ/SQpiUnbwxlCoBJWis3/8QnO2z2uEMnQ\n\
    RY9ilxqvAgMBAAECggEAFT+Phu572QZvYO8oSFWeBQzsGGFgVHDt9V2VxgeAdKZW\n\
    svZ7hrowtqOR3Y6RnlzhTs1CSu/znu1JFIXIh0O+dkkkQID/7fmR8yWuSELlTgAA\n\
    mUqU3qNusAIfm0mFPkM70r2K3gejvIvhG99u4xSq/FP8f77nMbNGNuTkRv5lr8lz\n\
    CxyBdsC8ZVimlfyzcBfI4YaumkyjZls58VeVJYK1PwIW+f9+QuHcv803JH1rnrH0\n\
    pyiOD04WuNfDbDKKBoT1k1IcfKGxLgoGknBoQUq9PXFxQL4NTD1zSQ+UzAVMjdnK\n\
    XdJ8MiUtpXXsR5uzSnT3fCoSnovWa0wAJuxg7bwScQKBgQDqgza1a0I4ZSGKBigm\n\
    iYaLbLAw2rqqFoSeN3j6o6pp6pVD77QFf11E7DtNwo3/oucV3dDO26ZqBYEgjh6E\n\
    IFgmuTCsObv68B7Ftpk9GbzO1z5C8wXDlZF/LGfc+/6VwUd030LFBxynXZiP+H5k\n\
    VGUn4GM425Uib67erICGZBjXhwKBgQC8iHm51qBIJdnxTeANK/vErnWppI/+ylMS\n\
    55F6E0+08DU0CmeADIYldqmInToffZ5OxXuH+9CB46ZXzU/jEZlGzAV+BCPWzNWn\n\
    n9MPTPX1dTa2fcbya6+BbYY5vTAq28YErQSSGtshmXP7chLbDn1YHAMTYiFXT3Nd\n\
    4vN9cwwdmQKBgC+n9ogrgcuAEa7AzYv3SpYuwidLa7+Az/ZvA3C7rbfUafziS00K\n\
    suN8thh5Pu4CJr+00egXjtvgvnrlJqJnCELmQ1Pcb6zw+u/myYE3J71I1Gg6hqNW\n\
    U7jqRzkLJJM+Mc04SZNMfLAuPwCS2Ox+kgfme4NkbAwRchHiMEQfN0UlAoGBAJ0X\n\
    Hk4RnoXfPCTLHaf8ARFVyGas3uqPDP9Qgi/0jVKXX81X6fuQ8+mdYQ+a6kY49kJu\n\
    XErnMEo6NG0ufqUWEeAiuWTR8EbXcCL2kfdKyhpXK1qNnMp7ncY7Sz8q+0qKywk0\n\
    XP2ccQAjC7f0ye9KuHhFBu8Msfk9E0SkiPv7c5npAoGBALX09sBaqp+Tb4oYiSIf\n\
    aNqSMgqhLsQr2VHfM3jnErVchjfw1sWTcsXz2vG84C9FrGZCwWKZusruE+oF5XMM\n\
    UWIi20aAXHbs/49MOLCbJhK+iyRWBt7+bcgpru7wnRvXfFD7J2QO2dnWt1/+rAPY\n\
    xKIcTw4f/h0aTGFz60pq+INn\n\
    -----END PRIVATE KEY-----\n";

/// The public half of [`TEST_PRIVATE_KEY`]: a PKCS#1 `RSAPublicKey`, DER, base64.
const TEST_PUBLIC_KEY: &str = "\
    MIIBCgKCAQEArLVhZI1waTp26lTwJABUKJs68ANTFXihfV269pxaLLqrTPe6kclvkGBIK/Sru8De3+s07vVBJacJ\
    bHQcezLSsZjEI9qMfiiS9i1ypbVv8nPXyDiVY/2AibGQurnBEI6PYc0wBizzVnFmYsJ1/8udWkvIHI5VTLDoeobI\
    uDwXsPwvpt5lIyg60pR7khzB+V2kEBXCpwS8mEgxBzaamG3PR7xW5XQEXiP7uhwk4ek1Yy66DIYx0RZnCPOJHrE+\
    bhO4hc4M/edequ1l5B9bylRb+VtwEHrcXj3YEqOhbST93SqxEP0kKYlJ28MZQqASVorN//EJzts9rhDJ0EWPYpca\
    rwIDAQAB";

/// The `client_email` of [`service_account_key_json`].
pub(crate) const TEST_CLIENT_EMAIL: &str = "robot@gdrive-test.iam.gserviceaccount.com";

/// A service account key file, as the Google Cloud console creates it, whose token endpoint is
/// `token_uri`.
pub(crate) fn service_account_key_json(token_uri: &str) -> String {
    serde_json::json!({
        "type": "service_account",
        "project_id": "gdrive-test",
        "private_key_id": "0123456789abcdef0123456789abcdef01234567",
        "private_key": TEST_PRIVATE_KEY,
        "client_email": TEST_CLIENT_EMAIL,
        "client_id": "123456789012345678901",
        "auth_uri": "https://accounts.google.com/o/oauth2/auth",
        "token_uri": token_uri,
        "auth_provider_x509_cert_url": "https://www.googleapis.com/oauth2/v1/certs",
        "client_x509_cert_url": "https://www.googleapis.com/robot/v1/metadata/x509/robot%40gdrive-test.iam.gserviceaccount.com",
        "universe_domain": "googleapis.com"
    })
    .to_string()
}

/// The claims of a JWT bearer assertion (RFC 7523) after checking that it is signed with
/// [`TEST_PRIVATE_KEY`] (RS256).
pub(crate) fn verified_jwt_claims(assertion: &str) -> serde_json::Value {
    use base64::Engine;

    let engine = base64::engine::general_purpose::URL_SAFE;
    let (signed, signature) = assertion.rsplit_once('.').expect("a JWT has three parts");
    let (header, claims) = signed.split_once('.').expect("a JWT has three parts");

    let header: serde_json::Value =
        serde_json::from_slice(&engine.decode(header).unwrap()).unwrap();
    assert_eq!(header["alg"], "RS256");

    let public_key = base64::engine::general_purpose::STANDARD
        .decode(TEST_PUBLIC_KEY)
        .unwrap();
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA256,
        public_key,
    )
    .verify(signed.as_bytes(), &engine.decode(signature).unwrap())
    .expect("the assertion is signed with the test key");

    serde_json::from_slice(&engine.decode(claims).unwrap()).unwrap()
}
