//! In-process HTTP/1.1 server standing in for the Drive API in tests.
//!
//! The server answers requests in order with canned responses and records every request it
//! receives, so tests can assert on the method, path, query and body gdrive sent. It speaks just
//! enough HTTP for the google-drive3 client: one request per connection (`Connection: close`) and
//! bodies delimited by `Content-Length`.
//!
//! Its URLs start with a path of its own ([`MockServer::root_url`]), and it answers only requests
//! below it: a connection from elsewhere that reaches its port (a test of another process, or of
//! this one, that probes a port it believes closed) gets a 404 and neither takes a canned
//! response nor shows up in [`MockServer::requests`]. Without that, such a stray request took the
//! first response, and the test's own request got the next one.

use std::{
    collections::VecDeque,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use crate::hub::Hub;

/// A request received by the mock server.
#[derive(Debug, Clone)]
pub(crate) struct Request {
    pub(crate) method: String,
    /// Path without the query string and without the server's own path, for example
    /// `/drive/v3/files/abc`.
    pub(crate) path: String,
    /// Raw (not percent-decoded) query pairs, in the order they were sent.
    pub(crate) query: Vec<(String, String)>,
    /// Header names are lowercase.
    pub(crate) headers: Vec<(String, String)>,
    pub(crate) body: Vec<u8>,
}

impl Request {
    pub(crate) fn query_param(&self, name: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn body_text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub(crate) fn body_json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("request body should be JSON")
    }
}

/// A canned response.
#[derive(Debug, Clone)]
pub(crate) struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
    /// Send only this many bytes of the body, then close the connection (the head still
    /// announces the whole body).
    cut_after: Option<usize>,
}

impl Response {
    pub(crate) fn json(status: u16, body: &serde_json::Value) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_string(),
            cut_after: None,
        }
    }

    /// A response whose body is sent as is, e.g. file content for a media download.
    pub(crate) fn raw(status: u16, body: &str) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: body.to_owned(),
            cut_after: None,
        }
    }

    /// `{root}` in `value` is replaced by the server's root URL ([`MockServer::root_url`]), for
    /// headers that point back at the server such as a resumable upload's `Location`.
    pub(crate) fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    /// The connection breaks after `bytes` bytes of the body, as when a connection is reset or
    /// an HTTP/2 stream is ended by a GOAWAY in the middle of a download.
    pub(crate) fn cut_after(mut self, bytes: usize) -> Self {
        self.cut_after = Some(bytes);
        self
    }
}

pub(crate) struct MockServer {
    addr: SocketAddr,
    /// `/mock-<random>/`: the path every URL of this server starts with.
    root_path: String,
    requests: Arc<Mutex<Vec<Request>>>,
    task: JoinHandle<()>,
}

impl MockServer {
    /// Starts a server that answers the n-th request below its root URL with `responses[n]`,
    /// and with a 500 once the responses are used up.
    pub(crate) async fn start(responses: Vec<Response>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock server");
        let addr = listener.local_addr().expect("mock server address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let root_path = format!("/mock-{:016x}/", fastrand::u64(..));

        let recorded = Arc::clone(&requests);
        let root_url = format!("http://{addr}{root_path}");
        let own_path = root_path.clone();
        let mut responses = VecDeque::from(responses);
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let Some(mut request) = read_request(&mut stream).await else {
                    continue;
                };
                // Keep the `/` that starts the path below the server's own.
                let own = request
                    .path
                    .strip_prefix(&own_path[..own_path.len() - 1])
                    .filter(|path| path.starts_with('/'));
                let Some(path) = own else {
                    let stray = Response::json(404, &serde_json::json!({"error": "not this mock"}));
                    write_response(&mut stream, &stray, &root_url).await;
                    continue;
                };
                request.path = path.to_owned();
                recorded.lock().expect("requests lock").push(request);

                let response = responses.pop_front().unwrap_or_else(|| {
                    Response::json(500, &serde_json::json!({"error": "unexpected request"}))
                });
                write_response(&mut stream, &response, &root_url).await;
            }
        });

        Self {
            addr,
            root_path,
            requests,
            task,
        }
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The path every URL of this server starts with, `/mock-<random>/`: the root of
    /// [`Self::root_url`] for a URL with another host name for the same address.
    pub(crate) fn root_path(&self) -> &str {
        &self.root_path
    }

    /// `http://127.0.0.1:PORT/mock-<random>/`, the root URL of a hub for this server.
    pub(crate) fn root_url(&self) -> String {
        format!("http://{}{}", self.addr, self.root_path)
    }

    /// A hub whose requests all go to this server, without credentials.
    pub(crate) fn hub(&self) -> Hub {
        Hub::for_mock_server(&self.root_url()).expect("create mock hub")
    }

    pub(crate) fn requests(&self) -> Vec<Request> {
        self.requests.lock().expect("requests lock").clone()
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn read_request(stream: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let method = request_line.next()?.to_owned();
    let target = request_line.next()?.to_owned();

    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let content_length = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);

    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_length {
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }

    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let query = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (key.to_owned(), value.to_owned())
        })
        .collect();

    Some(Request {
        method,
        path: path.to_owned(),
        query,
        headers,
        body,
    })
}

async fn write_response(stream: &mut TcpStream, response: &Response, root_url: &str) {
    let mut head = format!(
        "HTTP/1.1 {} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
        Connection: close\r\n",
        response.status,
        response.body.len()
    );
    for (name, value) in &response.headers {
        let value = value.replace("{root}", root_url);
        head.push_str(name);
        head.push_str(": ");
        head.push_str(&value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let _ = stream.write_all(head.as_bytes()).await;
    let body = response.body.as_bytes();
    match response.cut_after {
        Some(bytes) => {
            let _ = stream.write_all(&body[..bytes.min(body.len())]).await;
            let _ = stream.flush().await;
            // Let the client read the bytes before the connection drops.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        None => {
            let _ = stream.write_all(body).await;
        }
    }
    let _ = stream.shutdown().await;
}

#[cfg(test)]
mod tests {
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpStream,
    };

    use super::{MockServer, Response};

    /// The reply to `GET <target>` on a new connection to `server`.
    async fn get(server: &MockServer, target: &str) -> String {
        let mut connection = TcpStream::connect(server.addr()).await.unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nHost: mock\r\nConnection: close\r\n\r\n");
        connection.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        connection.read_to_string(&mut reply).await.unwrap();
        reply
    }

    #[tokio::test]
    async fn requests_from_elsewhere_take_no_response() {
        let server = MockServer::start(vec![Response::raw(200, "mine")]).await;
        let root = server.root_path().trim_end_matches('/').to_owned();

        // A request of someone who took the port for a closed one, and one whose path only
        // starts like the server's.
        for stray in [
            "/computeMetadata/v1/token",
            &format!("{root}0/drive/v3/files"),
        ] {
            let reply = get(&server, stray).await;
            assert!(reply.starts_with("HTTP/1.1 404 "), "{reply}");
        }
        assert!(server.requests().is_empty());

        let reply = get(&server, &format!("{root}/drive/v3/files?q=x")).await;
        assert!(reply.starts_with("HTTP/1.1 200 "), "{reply}");
        assert!(reply.ends_with("\r\n\r\nmine"), "{reply}");
        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path, "/drive/v3/files");
        assert_eq!(requests[0].query_param("q"), Some("x"));
    }
}
