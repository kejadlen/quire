use std::net::SocketAddr;
use std::os::unix::net::UnixListener as StdUnixListener;

use axum::Router;
use axum::routing::get;
use miette::{Context, IntoDiagnostic, Result};
use quire::Quire;
use quire::ci;
use quire::event::PushEvent;
use quire::mirror;

async fn health() -> &'static str {
    "ok"
}

async fn index() -> &'static str {
    "quire\n"
}

pub async fn run(quire: &Quire) -> Result<()> {
    let addr: SocketAddr = ([0, 0, 0, 0], 3000).into();

    // Set up event socket.
    let socket_path = quire.socket_path();

    // Clean up stale socket from previous run.
    if socket_path.exists() {
        fs_err::remove_file(&socket_path).into_diagnostic()?;
    }

    let std_listener = StdUnixListener::bind(&socket_path)
        .into_diagnostic()
        .context(format!(
            "failed to bind event socket at {}",
            socket_path.display()
        ))?;
    std_listener.set_nonblocking(true).into_diagnostic()?;
    let listener = tokio::net::UnixListener::from_std(std_listener).into_diagnostic()?;

    tracing::info!(path = %socket_path.display(), "listening on event socket");

    // Scan for orphaned runs from a previous server instance.
    for repo in quire.repos().context("failed to list repos")? {
        repo.runs().reconcile_orphans()?;
    }

    let quire_handle = quire.clone();
    let event_handle = tokio::spawn(event_listener(listener, quire_handle));

    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(index));

    tracing::info!(%addr, "starting HTTP server");

    let tcp_listener = tokio::net::TcpListener::bind(addr)
        .await
        .into_diagnostic()?;

    // Run HTTP server. When it finishes, abort the event listener.
    let result = axum::serve(tcp_listener, app).await.into_diagnostic();
    event_handle.abort();
    // Clean up socket on shutdown.
    let _ = fs_err::remove_file(&socket_path);
    result
}

async fn event_listener(listener: tokio::net::UnixListener, quire: Quire) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let quire = quire.clone();
                tokio::spawn(handle_event_connection(stream, quire));
            }
            Err(e) => {
                tracing::error!(%e, "failed to accept event connection");
            }
        }
    }
}

async fn handle_event_connection(mut stream: tokio::net::UnixStream, quire: Quire) {
    use tokio::io::AsyncBufReadExt;

    let (reader, _writer) = stream.split();
    let mut reader = tokio::io::BufReader::new(reader);
    let mut line = String::new();

    match reader.read_line(&mut line).await {
        Ok(0) => return, // empty connection, ignore
        Ok(_) => {}
        Err(e) => {
            tracing::error!(%e, "failed to read event from socket");
            return;
        }
    }

    let event: PushEvent = match serde_json::from_str(&line) {
        Ok(e) => e,
        Err(e) => {
            tracing::error!(%e, "failed to parse push event");
            return;
        }
    };

    tracing::info!(repo = %event.repo, r#type = %event.r#type, "received event");

    if event.r#type != "push" {
        tracing::warn!(r#type = %event.r#type, "unknown event type, ignoring");
        return;
    }

    mirror::push(&quire, &event).await;
    ci::trigger(&quire, &event);
}
