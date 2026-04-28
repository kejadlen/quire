# Mirror push from hook to serve via event socket

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move the mirror `git push` out of the `quire hook post-receive` process and into the `quire serve` event loop over a Unix domain socket.

**Architecture:** The hook parses stdin refs as before, but instead of pushing directly, it builds a JSON event (`{type:"push", repo, pushed_at, refs:[...]}`), connects to `/var/quire/server.sock`, writes one line, and exits. `quire serve` binds the socket on startup; a listener task per connection parses the event, looks up the repo's mirror config, and runs the mirror `git push` from inside the server process. Mirror failures surface in `quire serve` logs. If `quire serve` is not running, the hook emits a clear stderr warning and exits cleanly (no run created).

**Tech stack:** tokio (UnixStream/UnixListener), serde_json, existing Quire/Repo types

---

## File structure

| File | Change | Responsibility |
|------|--------|---------------|
| `src/event.rs` | Create | Push event types (`PushEvent`, `PushRef`) and socket path constant |
| `src/quire.rs` | Modify | Add `socket_path()` method |
| `src/lib.rs` | Modify | Export `event` module |
| `src/bin/quire/commands/hook.rs` | Modify | Replace direct mirror push with socket send |
| `src/bin/quire/commands/serve.rs` | Modify | Bind event socket, spawn listener task |
| `src/error.rs` | Modify | Add `EventSocket` error variant |

---

### Task 1: Define event types and socket path

**Files:**
- Create: `src/event.rs`
- Modify: `src/lib.rs`
- Modify: `src/error.rs`
- Modify: `src/quire.rs`

- [ ] **Step 1: Create `src/event.rs` with push event types and socket path**

```rust
use std::path::PathBuf;

/// Path to the event socket created by `quire serve`.
pub fn socket_path() -> PathBuf {
    std::path::PathBuf::from("/var/quire/server.sock")
}

/// A single ref update from a push.
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PushRef {
    pub r#ref: String,
    pub old_sha: String,
    pub new_sha: String,
}

/// A push event sent from hook to serve over the event socket.
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PushEvent {
    pub r#type: String,
    pub repo: String,
    pub pushed_at: String,
    pub refs: Vec<PushRef>,
}

/// Build a push event from the hook's parsed refs.
///
/// `repo` is the repo name relative to the repos dir (e.g. "foo.git").
/// `pushed_at` is ISO 8601 UTC.
pub fn build_push_event(repo: String, refs: Vec<PushRef>) -> PushEvent {
    PushEvent {
        r#type: "push".to_string(),
        repo,
        pushed_at: chrono_now_iso(),
        refs,
    }
}

fn chrono_now_iso() -> String {
    // Use a simple format without pulling in chrono.
    // The hook runs for a few milliseconds; second precision is fine.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| format!("{}", d.as_secs()))
        .unwrap_or_else(|_| "unknown".to_string())
}
```

Wait — no `chrono` dependency. Let me use a simpler timestamp. Actually, looking at Cargo.toml there's no chrono. The task description says `pushed_at` but doesn't mandate ISO 8601. Use a Unix timestamp as a string, which is unambiguous and doesn't need a dependency.

Actually, let me reconsider. The task spec says `pushed_at` — let's use RFC 3339 from `time` or just a Unix epoch seconds string. Simplest: Unix epoch seconds as a string. No new dependency needed.

```rust
use std::path::PathBuf;

/// Path to the event socket created by `quire serve`.
pub fn socket_path() -> PathBuf {
    PathBuf::from("/var/quire/server.sock")
}

/// A single ref update from a push.
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PushRef {
    pub r#ref: String,
    pub old_sha: String,
    pub new_sha: String,
}

/// A push event sent from hook to serve over the event socket.
#[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct PushEvent {
    pub r#type: String,
    pub repo: String,
    pub pushed_at: String,
    pub refs: Vec<PushRef>,
}

/// Build a push event from parsed refs.
///
/// `repo` is the repo name relative to the repos dir (e.g. "foo.git").
/// `pushed_at` is seconds since Unix epoch.
pub fn build_push_event(repo: String, refs: Vec<PushRef>) -> PushEvent {
    let pushed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_else(|_| "0".to_string());

    PushEvent {
        r#type: "push".to_string(),
        repo,
        pushed_at,
        refs,
    }
}
```

- [ ] **Step 2: Add `EventSocket` error variant to `src/error.rs`**

Add to the `Error` enum:

```rust
#[error("event socket error: {0}")]
EventSocket(String),
```

- [ ] **Step 3: Add `socket_path()` method to `Quire` in `src/quire.rs`**

```rust
pub fn socket_path(&self) -> PathBuf {
    self.base_dir.join("server.sock")
}
```

- [ ] **Step 4: Export event module in `src/lib.rs`**

Add `pub mod event;` to the module declarations.

- [ ] **Step 5: Run `cargo check` to verify compilation**

Run: `cargo check --workspace`
Expected: compiles without errors

---

### Task 2: Hook sends event to socket instead of pushing directly

**Files:**
- Modify: `src/bin/quire/commands/hook.rs`

- [ ] **Step 1: Rewrite `post_receive` to build event and send to socket**

The new `post_receive` should:

1. Read stdin lines (same as before) into `PushRef` structs (keep the zero-sha filter).
2. If no refs, return early.
3. Resolve the repo name from `GIT_DIR` relative to `repos_dir`.
4. Build a `PushEvent` via `quire::event::build_push_event`.
5. Serialize to JSON, append `\n`.
6. Try to connect to the socket at `quire.socket_path()`.
   - If the socket doesn't exist (serve not running), print a clear warning to stderr and return `Ok(())`.
   - If connection succeeds, write the line and close.
7. No mirror lookup, no token, no `push_to_mirror` call.

The hook becomes a thin "collect refs → serialize → write to socket" pipeline. All mirror logic moves to the server side.

Key implementation detail: the hook is synchronous (it's called by git), but needs to connect to a Unix socket. Use `std::os::unix::net::UnixStream::connect` for a blocking connect + write.

Here's the rewritten function:

```rust
fn post_receive(quire: &Quire) -> Result<()> {
    if io::stdin().is_terminal() {
        bail!("quire hook is for git to invoke, not for direct CLI use");
    }

    let git_dir = std::env::var("GIT_DIR")
        .map_err(|e| miette!("GIT_DIR not set — hook must run inside a bare repo: {e}"))
        .and_then(|git_dir| {
            std::path::Path::new(&git_dir)
                .canonicalize()
                .into_diagnostic()
        })
        .map_err(|e| miette!("failed to resolve GIT_DIR: {e}"))?;

    let repo = quire
        .repo_from_path(&git_dir)
        .context("hook running in unrecognized repo")?;
    ensure!(
        repo.exists(),
        "GIT_DIR points to a non-existent repo: {}",
        git_dir.display()
    );

    // Parse pushed refs from stdin. Each line is:
    //   <old-sha> <new-sha> <refname>
    let stdin = io::stdin();
    let mut refs: Vec<quire::event::PushRef> = Vec::new();
    for line in stdin.lines() {
        let line = line.map_err(|e| miette!("failed to read hook stdin: {e}"))?;
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 3 {
            continue;
        }
        refs.push(quire::event::PushRef {
            old_sha: parts[0].to_string(),
            new_sha: parts[1].to_string(),
            r#ref: parts[2].to_string(),
        });
    }

    // Filter out deletions (all-zero new sha). Do this after collecting
    // so the event socket sees the full picture in the future.
    let has_updates = refs.iter().any(|r| r.new_sha != ZERO_SHA);

    if !has_updates {
        return Ok(());
    }

    // Resolve repo name relative to repos dir for the event payload.
    let repo_name = repo
        .path()
        .strip_prefix(quire.repos_dir())
        .map_err(|_| miette!("repo path not under repos dir"))?
        .to_string_lossy()
        .to_string();

    let event = quire::event::build_push_event(repo_name, refs);
    let mut line = serde_json::to_string(&event)
        .into_diagnostic()
        .context("failed to serialize push event")?;
    line.push('\n');

    let socket_path = quire.socket_path();
    if !socket_path.exists() {
        eprintln!(
            "quire: server not running ({}), skipping event",
            socket_path.display()
        );
        return Ok(());
    }

    let mut stream = std::os::unix::net::UnixStream::connect(&socket_path)
        .into_diagnostic()
        .context("failed to connect to event socket")?;
    io::Write::write_all(&mut stream, line.as_bytes())
        .into_diagnostic()
        .context("failed to write event to socket")?;

    tracing::info!(repo = %event.repo, "push event sent to server");
    Ok(())
}

const ZERO_SHA: &str = "0000000000000000000000000000000000000000";
```

- [ ] **Step 2: Update imports in `hook.rs`**

Add `serde_json` to the imports (it's already available via axum's dependency tree — need to add it to `Cargo.toml` explicitly).

- [ ] **Step 3: Run `cargo check` to verify**

Run: `cargo check --workspace`
Expected: compiles without errors

---

### Task 3: Add serde_json to Cargo.toml

**Files:**
- Modify: `Cargo.toml`

- [ ] **Step 1: Add serde_json dependency**

Add `serde_json = "*"` to `[dependencies]` in `Cargo.toml`.

- [ ] **Step 2: Run `cargo check`**

Run: `cargo check --workspace`

---

### Task 4: Serve listens on event socket and dispatches mirror pushes

**Files:**
- Modify: `src/bin/quire/commands/serve.rs`

- [ ] **Step 1: Add event socket listener to `serve::run`**

The listener should:

1. Bind a Unix listener at `quire.socket_path()`.
2. Clean up any stale socket file before binding.
3. Spawn a task that accepts connections and reads one line from each.
4. Parse the line as JSON `PushEvent`.
5. If `event.type == "push"`, look up the repo's mirror config.
6. If mirror is configured, run the mirror push in a spawned blocking task.
7. Log mirror failures; don't crash the server.

```rust
use std::net::SocketAddr;
use std::os::unix::net::UnixListener as StdUnixListener;

use axum::Router;
use axum::routing::get;
use miette::{IntoDiagnostic, Result, miette};
use quire::Quire;
use tokio::net::UnixListener;

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
        std::fs::remove_file(&socket_path).into_diagnostic()?;
    }

    let std_listener = StdUnixListener::bind(&socket_path)
        .into_diagnostic()
 .map_err(|e| miette!("failed to bind event socket at {}: {e}", socket_path.display()))?;
    std_listener.set_nonblocking(true).into_diagnostic()?;
    let listener = UnixListener::from_std(std_listener).into_diagnostic()?;

    tracing::info!(path = %socket_path.display(), "listening on event socket");

    let quire_clone = quire.clone();
    let event_handle = tokio::spawn(event_listener(listener, quire_clone));

    let app = Router::new()
        .route("/health", get(health))
        .route("/", get(index));

    tracing::info!(%addr, "starting HTTP server");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .into_diagnostic()?;

    // Run HTTP server. When it finishes, abort the event listener.
    let result = axum::serve(listener, app).await.into_diagnostic();
    event_handle.abort();
    // Clean up socket on shutdown.
    let _ = std::fs::remove_file(&socket_path);
    result
}

async fn event_listener(listener: UnixListener, quire: &'static Quire) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                tokio::spawn(handle_event_connection(stream, quire));
            }
            Err(e) => {
                tracing::error!(%e, "failed to accept event connection");
            }
        }
    }
}

async fn handle_event_connection(
    mut stream: tokio::net::UnixStream,
    quire: &Quire,
) {
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

    dispatch_push(quire, &event).await;
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
    let refs: Vec<&str> = event
        .refs
        .iter()
        .filter(|r| r.new_sha != "0000000000000000000000000000000000000000")
        .map(|r| r.r#ref.as_str())
        .collect();

    if refs.is_empty() {
        return;
    }

    tracing::info!(url = %mirror.url, refs = ?refs, "pushing to mirror");
    let result = tokio::task::spawn_blocking(move || {
        repo.push_to_mirror(&mirror, &token, &refs)
    })
    .await;

    match result {
        Ok(Ok(())) => tracing::info!(url = %mirror.url, "mirror push complete"),
        Ok(Err(e)) => tracing::error!(url = %mirror.url, %e, "mirror push failed"),
        Err(e) => tracing::error!(url = %mirror.url, %e, "mirror push task panicked"),
    }
}
```

Wait — `&'static Quire` won't work because `Quire` is stack-allocated in `main`. Need to `Box::leak` or use `Arc`. Actually, looking at the current code, `serve::run` takes `&Quire` already. The simplest approach that matches the existing pattern: leak a `Box<Quire>` to get a `&'static Quire`. At process lifetime, this is fine.

Actually, let me re-examine. The `event_listener` function needs a reference that outlives the spawned task. The `Quire` is created in `main` and lives for the entire process. We can box it and get a `'static` reference. But that changes the API for all commands.

Simpler approach: wrap `Quire` in an `Arc` and share it. Or even simpler: since `Quire` is just a `PathBuf`, we can just create a new one inside the spawned task. `Quire::default()` is cheap.

Actually the cleanest is: `Quire` is small (one `PathBuf`), `Clone` is trivial. Let me add `Clone` derive to `Quire`, then move it into the spawned task. But `&Quire` is passed to `serve::run`... the simplest change: just construct a new `Quire` inside the task from the same base_dir.

Let me think about this differently. The `event_listener` doesn't need `&'static Quire`. It needs an owned value or something that implements `Clone + Send + 'static`. If I derive `Clone` on `Quire` (trivial since it's just a `PathBuf`), then:

```rust
let quire_for_listener = quire.clone();
let event_handle = tokio::spawn(async move {
    event_listener(listener, quire_for_listener).await;
});
```

But then `event_listener` takes an owned `Quire`, and `handle_event_connection` needs a reference to it. This works with `&Quire` in the closure since the `Quire` is owned by the `event_listener` future.

Wait, the `tokio::spawn` inside `event_listener` also needs references. Let me just use `Arc<Quire>`.

Hmm, looking at the existing code more carefully:

```rust
pub async fn run(_quire: &Quire) -> Result<()> {
```

And in main:
```rust
let quire = Quire::default();
commands::serve::run(&quire).await?
```

The simplest approach: derive `Clone` on `Quire` (it's just a PathBuf), and pass cloned instances into spawned tasks. No Arc needed.

- [ ] **Step 2: Derive `Clone` on `Quire` in `src/quire.rs`**

Add `#[derive(Clone)]` to `struct Quire`.

- [ ] **Step 3: Run `cargo check`**

Run: `cargo check --workspace`

---

### Task 5: Write tests

**Files:**
- Create: `src/event.rs` tests (inline module)
- Modify: `src/bin/quire/commands/hook.rs` (tests if any)
- Create integration test for event socket round-trip

- [ ] **Step 1: Add unit tests to `src/event.rs`**

Test `build_push_event` produces correct structure, `socket_path` returns expected path.

- [ ] **Step 2: Write integration test for hook → socket → mirror push**

Test that:
1. Hook sends event to socket.
2. Server listener receives event.
3. Mirror push executes from server side.
4. Hook logs warning when socket doesn't exist.

This test will set up a temp dir with repos, config, socket, and verify the round-trip.

- [ ] **Step 3: Run all tests**

Run: `cargo test --workspace`
Expected: all tests pass

---

### Task 6: Verify full build and commit

- [ ] **Step 1: Run `just all`**

Run: `just all`
Expected: fmt, clippy, test all pass

- [ ] **Step 2: Commit**

Commit message:

```
Move mirror push from hook to quire serve via event socket

Hook no longer invokes git push directly. Instead it reads stdin
triples, builds a JSON push event, and sends it over a Unix domain
socket to quire serve. The server listener parses the event, looks
up mirror config, and runs the push in-process.

When quire serve is not running, the hook prints a warning and
exits cleanly.

Assisted-by: pi <pi@shire>
```
