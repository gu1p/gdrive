//! A minimal HTTP/1.1 server for protocol tests. It answers the requests it accepts, one per
//! connection, with canned JSON bodies in order and records each request target, so tests can
//! check the parameters gdrive sends without calling the real Google API.

use std::{io, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

use crate::hub::Hub;

/// Upper bound for a test exchange, so a missing or extra request fails instead of hanging.
pub(crate) const TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) struct StubServer {
    base_url: String,
    handle: JoinHandle<io::Result<Vec<Request>>>,
}

/// A request as the server saw it.
#[derive(Debug)]
pub(crate) struct Request {
    pub(crate) path: String,
    pub(crate) params: Vec<(String, String)>,
}

impl Request {
    /// The value of a query parameter, if it was sent.
    pub(crate) fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

impl StubServer {
    /// Starts a server that answers one request with each body, in order.
    pub(crate) async fn start(bodies: Vec<String>) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base_url = format!("http://{}/drive/v3/", listener.local_addr()?);

        let handle = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(bodies.len());
            for body in bodies {
                let (mut stream, _) = listener.accept().await?;
                requests.push(read_request(&mut stream).await?);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await?;
                stream.shutdown().await?;
            }
            Ok(requests)
        });

        Ok(Self { base_url, handle })
    }

    /// A hub without credentials that sends every Drive request to this server.
    pub(crate) fn hub(&self) -> io::Result<Hub> {
        Hub::with_base_url(self.base_url.clone())
    }

    /// Waits until every body has been served and returns the requests in order.
    pub(crate) async fn requests(self) -> Vec<Request> {
        tokio::time::timeout(TIMEOUT, self.handle)
            .await
            .expect("stub server should receive one request per body")
            .expect("stub server task should not panic")
            .expect("stub server should not fail")
    }
}

async fn read_request(stream: &mut TcpStream) -> io::Result<Request> {
    let mut head = Vec::new();
    let mut buf = [0; 4096];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut buf).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        head.extend_from_slice(&buf[..read]);
    }

    let head = String::from_utf8_lossy(&head);
    let target = head
        .split_whitespace()
        .nth(1)
        .ok_or(io::ErrorKind::InvalidData)?;
    let url = url::Url::parse(&format!("http://stub{target}"))
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

    Ok(Request {
        path: url.path().to_owned(),
        params: url.query_pairs().into_owned().collect(),
    })
}
