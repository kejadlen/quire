use std::net::SocketAddr;
use std::os::unix::net::UnixListener as StdUnixListener;

use axum::Router;
use axum::routing::get;
use miette::{Context, IntoDiagnostic, Result};
use quire::Quire;
use quire::run;

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
        repo.runs().reconcile_orphans();
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

    let event: quire::event::PushEvent = match serde_json::from_str(&line) {
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

    dispatch_push(&quire, &event).await;
}

async fn dispatch_push(quire: &Quire, event: &quire::event::PushEvent) {
    let repo = match quire.repo(&event.repo) {
        Ok(r) if r.exists() => r,
        Ok(_) => {
            tracing::error!(repo = %event.repo, "repo not found on disk");
            return;
        }
        Err(e) => {
            tracing::error!(repo = %event.repo, %e, "invalid repo name in event");
            return;
        }
    };

    // CI gating: check each updated ref for .quire/ci.fnl.
    for push_ref in &event.refs {
        // Skip deletions (all-zero new sha).
        if push_ref.new_sha == "0000000000000000000000000000000000000000" {
            continue;
        }

        if repo.has_ci_fnl(&push_ref.new_sha) {
            let meta = run::RunMeta {
                sha: push_ref.new_sha.clone(),
                r#ref: push_ref.r#ref.clone(),
                pushed_at: event.pushed_at.clone(),
            };

            let runs = repo.runs();
            match runs.create(&meta) {
                Ok(mut run) => {
                    tracing::info!(
                        run_id = %run.id(),
                        sha = %push_ref.new_sha,
                        r#ref = %push_ref.r#ref,
                        "created CI run"
                    );

                    // No eval yet — immediately complete.
                    if let Err(e) = run.transition(run::RunState::Complete) {
                        tracing::error!(
                            run_id = %run.id(),
                            %e,
                            "failed to transition run to complete"
                        );
                    }
                }
                Err(e) => {
                    tracing::error!(
                        repo = %event.repo,
                        %e,
                        "failed to create CI run"
                    );
                }
            }
        }
    }

    // Mirror push — proceeds regardless of CI.
    let config = match repo.config() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(repo = %event.repo, %e, "failed to load repo config");
            return;
        }
    };

    let Some(mirror) = config.mirror else {
        tracing::debug!(repo = %event.repo, "no mirror configured, skipping");
        return;
    };

    let global_config = match quire.global_config() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(%e, "failed to load global config for mirror push");
            return;
        }
    };

    let token = match global_config.github.token.reveal() {
        Ok(t) => t.to_string(),
        Err(e) => {
            tracing::error!(%e, "failed to resolve GitHub token");
            return;
        }
    };

    // Only push refs that were actually updated (non-zero new sha).
    let refs: Vec<String> = event
        .refs
        .iter()
        .filter(|r| r.new_sha != "0000000000000000000000000000000000000000")
        .map(|r| r.r#ref.clone())
        .collect();

    if refs.is_empty() {
        return;
    }

    let mirror_url = mirror.url.clone();
    tracing::info!(url = %mirror.url, refs = ?refs, "pushing to mirror");

    let result = tokio::task::spawn_blocking(move || {
        let ref_slices: Vec<&str> = refs.iter().map(|s| s.as_str()).collect();
        repo.push_to_mirror(&mirror, &token, &ref_slices)
    })
    .await;

    match result {
        Ok(Ok(())) => tracing::info!(url = %mirror_url, "mirror push complete"),
        Ok(Err(e)) => tracing::error!(url = %mirror_url, %e, "mirror push failed"),
        Err(e) => tracing::error!(url = %mirror_url, %e, "mirror push task panicked"),
    }
}
