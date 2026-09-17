//! Loopback control API for the native browser panel.
//!
//! The shell publishes a small JSON descriptor under `$DSH_HOME` and serves this
//! API on an ephemeral loopback port. A web UI (the dsh-web-review plugin) finds
//! the descriptor, drives the panel through it, and otherwise ignores it — the
//! shell stays independent of that plugin, and the harness is never touched.
//!
//! Endpoints (all JSON, loopback only by construction):
//!
//! | Method | Path             | Body / result                                  |
//! |--------|------------------|------------------------------------------------|
//! | GET    | `/panel/state`   | `{ open, session, url }`                       |
//! | POST   | `/panel/open`    | `OpenRequest`                                  |
//! | POST   | `/panel/bounds`  | `BoundsRequest`                                |
//! | POST   | `/panel/command` | `CommandRequest`                               |
//! | GET    | `/health`        | `{ ok, product, version }`                     |
//! | GET    | `/window/state`  | `{ fullscreen, escapesSwallowed }`             |
//! | POST   | `/window/fullscreen` | `{ value: bool }`                          |

use std::io::Read;
use std::path::PathBuf;
use std::thread;

use serde::Serialize;
use tauri::{AppHandle, Manager};
use tiny_http::{Header, Response, Server};

use crate::native_browser::{BoundsRequest, CommandRequest, NativeBrowser, OpenRequest};
use crate::window_guard;

/// Descriptor filename the shell advertises for clients to discover.
pub const DESCRIPTOR_NAME: &str = "native-browser.json";

#[derive(Serialize)]
struct Health<'a> {
    ok: bool,
    product: &'a str,
    version: &'a str,
}

#[derive(serde::Deserialize)]
struct FullscreenRequest {
    value: bool,
}

#[derive(Serialize)]
struct WindowState {
    fullscreen: bool,
    #[serde(rename = "escapesSwallowed")]
    escapes_swallowed: u64,
}

#[derive(Serialize)]
struct Ack {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<crate::native_browser::PanelState>,
}

fn json_response(body: String) -> Response<std::io::Cursor<Vec<u8>>> {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"application/json; charset=utf-8"[..])
        .expect("static header");
    Response::from_string(body).with_header(header)
}

fn read_body(request: &mut tiny_http::Request) -> String {
    let mut body = String::new();
    let _ = request.as_reader().take(1_048_576).read_to_string(&mut body);
    body
}

/// Resolve `$DSH_HOME` the same way the harness does, so both sides agree.
fn dsh_home() -> PathBuf {
    if let Ok(value) = std::env::var("DSH_HOME") {
        if !value.trim().is_empty() {
            return PathBuf::from(value);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".dsh")
}

/// Publish the loopback endpoint so a client can discover this shell.
pub fn advertise(port: u16) -> Result<PathBuf, String> {
    let dir = dsh_home().join("web-review");
    std::fs::create_dir_all(&dir).map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let path = dir.join(DESCRIPTOR_NAME);
    let payload = serde_json::json!({
        "schema": 1,
        "port": port,
        "pid": std::process::id(),
        "product": "deepseek-harness-desktop",
        "version": env!("CARGO_PKG_VERSION"),
        "capabilities": ["browser-panel"],
    });
    std::fs::write(&path, format!("{payload}\n"))
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(path)
}

/// Remove the descriptor on shutdown so a stale port is never advertised.
pub fn withdraw() {
    let _ = std::fs::remove_file(dsh_home().join("web-review").join(DESCRIPTOR_NAME));
}

/// Start the control server; returns the bound port.
pub fn start(app: AppHandle) -> Result<u16, String> {
    let server = Server::http("127.0.0.1:0").map_err(|error| format!("cannot bind the control API: {error}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .map(|address| address.port())
        .ok_or_else(|| "the control API has no TCP address".to_string())?;

    thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let path = request.url().split('?').next().unwrap_or("").to_string();
            let browser = app.state::<NativeBrowser>();
            let ack = match path.as_str() {
                "/health" => json_response(
                    serde_json::to_string(&Health {
                        ok: true,
                        product: "deepseek-harness-desktop",
                        version: env!("CARGO_PKG_VERSION"),
                    })
                    .unwrap_or_else(|_| "{\"ok\":false}".into()),
                ),
                "/panel/state" => {
                    let state = browser.state();
                    json_response(serde_json::to_string(&Ack { ok: true, error: None, state: Some(state) }).unwrap_or_default())
                }
                "/panel/open" => {
                    let body = read_body(&mut request);
                    let result = serde_json::from_str::<OpenRequest>(&body)
                        .map_err(|error| format!("invalid open request: {error}"))
                        .and_then(|parsed| browser.open(&app, &parsed));
                    json_response(serde_json::to_string(&Ack { ok: result.is_ok(), error: result.err(), state: None }).unwrap_or_default())
                }
                "/panel/bounds" => {
                    let body = read_body(&mut request);
                    let result = serde_json::from_str::<BoundsRequest>(&body)
                        .map_err(|error| format!("invalid bounds request: {error}"))
                        .and_then(|parsed| browser.bounds(&parsed));
                    json_response(serde_json::to_string(&Ack { ok: result.is_ok(), error: result.err(), state: None }).unwrap_or_default())
                }
                "/panel/command" => {
                    let body = read_body(&mut request);
                    let result = serde_json::from_str::<CommandRequest>(&body)
                        .map_err(|error| format!("invalid command request: {error}"))
                        .and_then(|parsed| browser.command(&parsed));
                    json_response(serde_json::to_string(&Ack { ok: result.is_ok(), error: result.err(), state: None }).unwrap_or_default())
                }
                "/window/state" => {
                    let fullscreen = app
                        .get_window("main")
                        .and_then(|window| window.is_fullscreen().ok())
                        .unwrap_or(false);
                    json_response(
                        serde_json::to_string(&WindowState {
                            fullscreen,
                            escapes_swallowed: window_guard::escapes_swallowed(),
                        })
                        .unwrap_or_default(),
                    )
                }
                "/window/fullscreen" => {
                    let body = read_body(&mut request);
                    let result = serde_json::from_str::<FullscreenRequest>(&body)
                        .map_err(|error| format!("invalid fullscreen request: {error}"))
                        .and_then(|parsed| {
                            app.get_window("main")
                                .ok_or_else(|| "the main window is not available".to_string())?
                                .set_fullscreen(parsed.value)
                                .map_err(|error| error.to_string())
                        });
                    json_response(serde_json::to_string(&Ack { ok: result.is_ok(), error: result.err(), state: None }).unwrap_or_default())
                }
                _ => Response::from_string("{\"ok\":false,\"error\":\"unknown endpoint\"}").with_status_code(404),
            };
            let _ = request.respond(ack);
        }
    });

    Ok(port)
}
