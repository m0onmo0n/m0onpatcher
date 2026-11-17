use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context as _, Result};
use log::{debug, error, info, warn};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tungstenite::protocol::{Role, WebSocket};
use tungstenite::Message;

use crate::game_data_reader;
use crate::live_context_manager::{self, LiveRenderContext};

const WEBSOCKET_PORT: u16 = 1997;
const REFRESH_COMMAND: &str = "refresh";

pub fn live_merge(
    source_html_path: &Path,
    output_html_path: &Path,
    source_directory: &Path,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let data = game_data_reader::read_from_file(source_html_path)?;
    info!(
        "Starting live merge of files in {} into {} with {} as reference",
        source_directory.display(),
        output_html_path.display(),
        source_html_path.display()
    );

    let extra_scripts = auto_reload_script().into_bytes();
    let mut context = live_context_manager::start_live_context(
        data,
        output_html_path,
        source_directory,
        extra_scripts,
    )?;

    let clients = start_websocket_server(WEBSOCKET_PORT, cancel.clone())?;

    let result = run_watch_loop(&mut context, &clients, &cancel);

    // Close all connected clients so peers see the socket close promptly,
    // and the accept thread has already exited since its listener is
    // non-blocking and checks `cancel`.
    clients.lock().unwrap().clear();
    result
}

/// File-system watcher: registers the five tracked subdirectories with a
/// single notify watcher, collects events into a debounce buffer, and
/// triggers a rebuild + broadcast 200ms after the last observed change —
/// same window the Kotlin implementation uses.
fn run_watch_loop(
    context: &mut LiveRenderContext,
    clients: &ClientList,
    cancel: &Arc<AtomicBool>,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<notify::Result<notify::Event>>();
    let mut watcher: RecommendedWatcher =
        notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })?;

    for dir in monitored_paths(context) {
        if dir.exists() {
            watcher
                .watch(&dir, RecursiveMode::NonRecursive)
                .with_context(|| format!("watch {}", dir.display()))?;
        } else {
            warn!("Monitored directory does not exist, skipping: {}", dir.display());
        }
    }

    let mut pending: HashSet<PathBuf> = HashSet::new();
    let mut last_change = Instant::now();

    loop {
        if cancel.load(Ordering::Relaxed) {
            info!("Live merge cancelled");
            return Ok(());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                if matches_tracked_change(&event.kind) {
                    for path in event.paths {
                        if pending.insert(path.clone()) {
                            let rel = path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("?")
                                .to_string();
                            info!(
                                "Detected change in {}/{}",
                                path.parent()
                                    .and_then(|p| p.file_name())
                                    .and_then(|n| n.to_str())
                                    .unwrap_or("?"),
                                rel
                            );
                        }
                        last_change = Instant::now();
                    }
                }
            }
            Ok(Err(err)) => warn!("watcher error: {err}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if !pending.is_empty()
                    && last_change.elapsed() >= Duration::from_millis(200)
                {
                    info!(
                        "200ms of stability after detecting {} changes, attempt build",
                        pending.len()
                    );
                    let paths: Vec<PathBuf> = pending.drain().collect();
                    if let Err(err) = live_context_manager::attempt_build(context, &paths) {
                        error!("Build failed: {err:#}");
                    }
                    broadcast(clients, REFRESH_COMMAND);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("watcher channel disconnected"));
            }
        }
    }
}

fn matches_tracked_change(kind: &EventKind) -> bool {
    use notify::event::{CreateKind, ModifyKind, RemoveKind};
    matches!(
        kind,
        EventKind::Create(CreateKind::File) | EventKind::Create(CreateKind::Any)
        | EventKind::Modify(ModifyKind::Any) | EventKind::Modify(ModifyKind::Data(_))
        | EventKind::Modify(ModifyKind::Metadata(_)) | EventKind::Modify(ModifyKind::Name(_))
        | EventKind::Remove(RemoveKind::File) | EventKind::Remove(RemoveKind::Any)
    )
}

fn monitored_paths(context: &LiveRenderContext) -> Vec<PathBuf> {
    let mut paths = vec![
        context.passages.directory.clone(),
        context.js.directory.clone(),
        context.css.directory.clone(),
    ];
    if let Some(sm) = &context.script_modules {
        paths.push(sm.directory.clone());
    }
    if let Some(stm) = &context.style_modules {
        paths.push(stm.directory.clone());
    }
    if context.system_scripts.libraries.is_some() || context.system_scripts.script.is_some() {
        paths.push(context.system_scripts.directory.clone());
    }
    paths
}

// ---- WebSocket broadcast server ----

type ClientList = Arc<Mutex<Vec<WebSocket<TcpStream>>>>;

fn start_websocket_server(port: u16, cancel: Arc<AtomicBool>) -> Result<ClientList> {
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
    let listener = TcpListener::bind(addr)
        .with_context(|| format!("bind ws server on {addr}"))?;
    // Non-blocking accept so the thread can poll `cancel` and exit cleanly
    // when live_merge is stopped; otherwise port 1997 would stay bound
    // until the GUI process exits.
    listener.set_nonblocking(true)?;
    info!("WebSocket server started on {addr}");

    let clients: ClientList = Arc::new(Mutex::new(Vec::new()));
    let clients_for_accept = clients.clone();

    thread::spawn(move || loop {
        if cancel.load(Ordering::Relaxed) {
            debug!("WebSocket accept thread exiting");
            return;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                // tungstenite's blocking handshake wants a blocking socket.
                if let Err(err) = stream.set_nonblocking(false) {
                    warn!("Failed to restore blocking mode on accepted socket: {err}");
                    continue;
                }
                match tungstenite::accept(stream) {
                    Ok(ws) => {
                        info!("WebSocket client connected");
                        clients_for_accept.lock().unwrap().push(ws);
                    }
                    Err(err) => warn!("WebSocket handshake failed: {err}"),
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(100));
            }
            Err(err) => warn!("WebSocket accept error: {err}"),
        }
    });

    Ok(clients)
}

fn broadcast(clients: &ClientList, message: &str) {
    let mut locked = clients.lock().unwrap();
    let before = locked.len();
    locked.retain_mut(|ws| match ws.send(Message::Text(message.into())) {
        Ok(()) => true,
        Err(err) => {
            debug!("Dropping WebSocket client after send error: {err}");
            false
        }
    });
    let dropped = before - locked.len();
    if dropped > 0 {
        debug!("Dropped {dropped} disconnected WebSocket client(s)");
    }
}

// ---- Injected JavaScript ----

fn auto_reload_script() -> String {
    let port = WEBSOCKET_PORT;
    let cmd = REFRESH_COMMAND;
    format!(
        r##"<script>
  try {{
    (() => {{
      const ws = new WebSocket("ws://localhost:{port}");
      let autoReload = sessionStorage.getItem("scRepackerReload") === "on";

      ws.onopen = () => {{
        console.log("Live Reload WebSocket opened");
      }};
      ws.onmessage = (message) => {{
        if (autoReload && message.data === "{cmd}") {{
          window.location.reload();
        }}
      }};

      const element = document.createElement("div");
      element.style.cssText = "position: absolute; z-index: 999; height: 20px; width: 150px; top: 7px; left: 14px; color: #111111; font-weight: bold; border: 2px solid #404040; text-align: center;";

      let reloadUpdated = () => {{
        if (autoReload) {{
          element.style.backgroundColor = "#55bd06";
          element.innerHTML = "Auto-reload ON";
          sessionStorage.setItem("scRepackerReload", "on");
        }} else {{
          element.style.backgroundColor = "#a0a0a0";
          element.innerHTML = "Auto-reload OFF";
          sessionStorage.removeItem("scRepackerReload");
        }}
      }};

      element.onclick = function() {{
        autoReload = !autoReload;
        reloadUpdated();
      }};

      reloadUpdated();
      document.body.appendChild(element);
    }})();
  }} catch (err) {{
    console.log("Error initializing the live reloader");
  }}
</script>"##
    )
}

// Role is re-exported in case someone wants to construct a raw WebSocket
// from a borrowed stream; unused directly here.
#[allow(dead_code)]
fn _role_check(_: Role, _: io::Result<()>) {}
