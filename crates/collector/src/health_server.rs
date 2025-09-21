use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use log::{debug, error, info};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

type ReadyFn = Arc<dyn Fn() -> bool + Send + Sync + 'static>;

async fn handle_connection(mut stream: TcpStream, ready_fn: ReadyFn) -> Result<()> {
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);

    // Parse the first request line: METHOD PATH HTTP/1.1
    let mut path = "/";
    if let Some(line) = req.lines().next() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() >= 2 {
            path = parts[1];
        }
    }

    let (status_line, body) = match path {
        "/live" => ("HTTP/1.1 200 OK\r\n", "live"),
        "/ready" => {
            if (ready_fn)() {
                ("HTTP/1.1 200 OK\r\n", "ready")
            } else {
                ("HTTP/1.1 503 Service Unavailable\r\n", "not ready")
            }
        }
        _ => ("HTTP/1.1 404 Not Found\r\n", "not found"),
    };

    let headers = format!(
        "{}Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        status_line,
        body.len()
    );
    if let Err(e) = stream.write_all(headers.as_bytes()).await {
        error!("Failed to write HTTP headers: {}", e);
        return Ok(());
    }
    if let Err(e) = stream.write_all(body.as_bytes()).await {
        error!("Failed to write HTTP body: {}", e);
    }
    let _ = stream.shutdown().await;
    Ok(())
}

pub async fn run(addr: String, ready_fn: ReadyFn, shutdown: CancellationToken) -> Result<()> {
    let addr: SocketAddr = addr.parse()?;
    let listener = TcpListener::bind(addr).await?;
    info!("Health server listening on {}", addr);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                debug!("Health server shutting down");
                break;
            }
            accept_res = listener.accept() => {
                match accept_res {
                    Ok((stream, _peer)) => {
                        let ready_fn = ready_fn.clone();
                        tokio::spawn(async move {
                            let _ = handle_connection(stream, ready_fn).await;
                        });
                    }
                    Err(e) => {
                        error!("Health server accept error: {}", e);
                    }
                }
            }
        }
    }
    Ok(())
}
