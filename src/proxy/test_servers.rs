//! In-process proxies for tests: an HTTP CONNECT proxy and a SOCKS5 proxy. Both record what
//! they were asked for and forward only to the loopback interface, so a test that goes wrong
//! never reaches another host.

use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};

/// What a proxy was asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProxyRequest {
    /// `host:port` of the CONNECT request, or the SOCKS5 destination.
    pub(crate) target: String,
    /// The `Proxy-Authorization` header, or the SOCKS5 `user:password`.
    pub(crate) auth: Option<String>,
}

pub(crate) struct TestProxy {
    addr: SocketAddr,
    requests: Arc<Mutex<Vec<ProxyRequest>>>,
    task: JoinHandle<()>,
}

impl TestProxy {
    /// An HTTP proxy that opens CONNECT tunnels to loopback targets. With `required_auth`, a
    /// request without that `Proxy-Authorization` gets a 407.
    pub(crate) async fn connect(required_auth: Option<&str>) -> TestProxy {
        let required_auth = required_auth.map(str::to_owned);
        Self::start(move |stream, requests| {
            let required_auth = required_auth.clone();
            async move { serve_connect(stream, &requests, required_auth.as_deref()).await }
        })
        .await
    }

    /// A SOCKS5 proxy (RFC 1928) for loopback targets that takes no authentication or the
    /// user name and password of RFC 1929.
    pub(crate) async fn socks5() -> TestProxy {
        Self::start(|stream, requests| async move { serve_socks5(stream, &requests).await }).await
    }

    async fn start<F, Fut>(serve: F) -> TestProxy
    where
        F: Fn(TcpStream, Arc<Mutex<Vec<ProxyRequest>>>) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Option<()>> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(serve(stream, Arc::clone(&recorded)));
            }
        });
        TestProxy {
            addr,
            requests,
            task,
        }
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub(crate) fn requests(&self) -> Vec<ProxyRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for TestProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve_connect(
    mut client: TcpStream,
    requests: &Mutex<Vec<ProxyRequest>>,
    required_auth: Option<&str>,
) -> Option<()> {
    let mut head = Vec::new();
    let mut byte = [0; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if client.read(&mut byte).await.ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next()?.split(' ');
    let (method, target) = (request_line.next()?, request_line.next()?.to_owned());
    let auth = lines
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("proxy-authorization"))
        .map(|(_, value)| value.trim().to_owned());
    requests.lock().unwrap().push(ProxyRequest {
        target: target.clone(),
        auth: auth.clone(),
    });

    if method != "CONNECT" {
        client
            .write_all(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            .await
            .ok()?;
        return None;
    }
    if required_auth.is_some() && auth.as_deref() != required_auth {
        client
            .write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\n\r\n")
            .await
            .ok()?;
        return None;
    }
    let Some(mut upstream) = connect_loopback(&target).await else {
        client
            .write_all(b"HTTP/1.1 403 Forbidden\r\n\r\n")
            .await
            .ok()?;
        return None;
    };
    client
        .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
        .await
        .ok()?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Some(())
}

async fn serve_socks5(mut client: TcpStream, requests: &Mutex<Vec<ProxyRequest>>) -> Option<()> {
    // Greeting: version 5, the offered methods.
    let mut header = [0; 2];
    client.read_exact(&mut header).await.ok()?;
    let mut methods = vec![0; usize::from(header[1])];
    client.read_exact(&mut methods).await.ok()?;

    let auth = if methods.contains(&2) {
        client.write_all(&[5, 2]).await.ok()?;
        // RFC 1929: version 1, user, password.
        let mut version_and_length = [0; 2];
        client.read_exact(&mut version_and_length).await.ok()?;
        let mut user = vec![0; usize::from(version_and_length[1])];
        client.read_exact(&mut user).await.ok()?;
        let mut length = [0; 1];
        client.read_exact(&mut length).await.ok()?;
        let mut password = vec![0; usize::from(length[0])];
        client.read_exact(&mut password).await.ok()?;
        client.write_all(&[1, 0]).await.ok()?;
        Some(format!(
            "{}:{}",
            String::from_utf8_lossy(&user),
            String::from_utf8_lossy(&password)
        ))
    } else {
        client.write_all(&[5, 0]).await.ok()?;
        None
    };

    // Request: version, CONNECT, reserved, address type, address, port.
    let mut request = [0; 4];
    client.read_exact(&mut request).await.ok()?;
    let host = match request[3] {
        1 => {
            let mut ip = [0; 4];
            client.read_exact(&mut ip).await.ok()?;
            std::net::Ipv4Addr::from(ip).to_string()
        }
        3 => {
            let mut length = [0; 1];
            client.read_exact(&mut length).await.ok()?;
            let mut name = vec![0; usize::from(length[0])];
            client.read_exact(&mut name).await.ok()?;
            String::from_utf8_lossy(&name).into_owned()
        }
        _ => return None,
    };
    let mut port = [0; 2];
    client.read_exact(&mut port).await.ok()?;
    let target = format!("{host}:{}", u16::from_be_bytes(port));
    requests.lock().unwrap().push(ProxyRequest {
        target: target.clone(),
        auth,
    });

    let Some(mut upstream) = connect_loopback(&target).await else {
        // Connection not allowed by ruleset.
        client
            .write_all(&[5, 2, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .ok()?;
        return None;
    };
    client
        .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
        .await
        .ok()?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Some(())
}

/// Connects to `target` if it is on the loopback interface; `localhost` is 127.0.0.1.
async fn connect_loopback(target: &str) -> Option<TcpStream> {
    let target = match target.strip_prefix("localhost:") {
        Some(port) => format!("127.0.0.1:{port}"),
        None => target.to_owned(),
    };
    let addr: SocketAddr = target.parse().ok()?;
    if !addr.ip().is_loopback() {
        return None;
    }
    TcpStream::connect(addr).await.ok()
}
