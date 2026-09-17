//! Native browser panel: a real WKWebView hosted inside the app window.
//!
//! This is the shell half of the "native browser" capability. It owns one child
//! webview positioned over the main view, loads any http(s) URL in it with a
//! persistent data store (so logins survive), and injects a bootstrap script
//! before page scripts on every navigation.
//!
//! Everything here is driven through the loopback control API in
//! `control_server.rs`; the shell knows nothing about the web UI that requests
//! it, and the harness itself is never modified.

use std::path::PathBuf;
use std::sync::Mutex;

use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, LogicalPosition, LogicalSize, Manager, Webview, WebviewBuilder, WebviewUrl};

/// Owns the single browser panel this shell can host.
#[derive(Default)]
pub struct NativeBrowser {
    panel: Mutex<Option<Panel>>,
}

struct Panel {
    session: String,
    /// Relay target this panel's message handler posts to.
    endpoint: Option<String>,
    /// The injected bootstrap this panel was created with: a caller that wants a
    /// different bootstrap needs a different panel, because init scripts are
    /// registered at creation and run on every navigation.
    bootstrap: String,
    webview: Webview<tauri::Wry>,
}

/// `POST /panel/open` body.
#[derive(Debug, Deserialize)]
pub struct OpenRequest {
    /// Caller-owned identity: a new session replaces the current panel.
    pub session: String,
    pub url: String,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Script injected before page scripts on every navigation.
    #[serde(default)]
    pub bootstrap: String,
    /// Loopback endpoint the panel's page messages are relayed to.
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// `POST /panel/bounds` body.
#[derive(Debug, Deserialize)]
pub struct BoundsRequest {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    /// Hide the panel without destroying it (for example while its tab is hidden).
    #[serde(default)]
    pub visible: Option<bool>,
}

/// `POST /panel/command` body.
#[derive(Debug, Deserialize)]
pub struct CommandRequest {
    /// One of `navigate`, `reload`, `back`, `forward`, `eval`, `close`.
    pub kind: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub script: Option<String>,
}

/// Where the panel actually sits, in logical window coordinates.
#[derive(Debug, Serialize)]
pub struct PanelBounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// `GET /panel/state` response.
#[derive(Debug, Serialize)]
pub struct PanelState {
    pub open: bool,
    pub session: Option<String>,
    pub url: Option<String>,
    /// What the shell applied, so a placement mismatch can be measured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<PanelBounds>,
    /// Whether the page-to-host relay is armed (the panel's only channel on an
    /// HTTPS page, where WebKit blocks a loopback request).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub relay: bool,
}

fn panel_label(session: &str) -> String {
    let clean: String = session
        .chars()
        .filter(|character| character.is_ascii_alphanumeric() || *character == '-')
        .take(48)
        .collect();
    format!("web-review-browser-{clean}")
}

impl NativeBrowser {
    /// Persistent web data directory: cookies and logins survive restarts.
    fn data_dir(app: &AppHandle) -> Result<PathBuf, String> {
        let dir = app
            .path()
            .app_data_dir()
            .map_err(|error| format!("cannot resolve the app data directory: {error}"))?
            .join("browser-panel");
        std::fs::create_dir_all(&dir)
            .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
        Ok(dir)
    }

    fn close_current(&self) -> Result<(), String> {
        let mut guard = self.panel.lock().map_err(|_| "panel lock poisoned")?;
        if let Some(panel) = guard.take() {
            crate::panel_bridge::set_endpoint(None);
            let _ = panel.webview.close();
        }
        Ok(())
    }

    /// Create or reuse the panel for one request.
    pub fn open(&self, app: &AppHandle, request: &OpenRequest) -> Result<(), String> {
        if request.width < 1.0 || request.height < 1.0 {
            return Err("panel needs a positive size".into());
        }
        let url: url::Url = request
            .url
            .parse()
            .map_err(|error| format!("invalid panel url: {error}"))?;
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err("panel url must be http(s)".into());
        }

        {
            let guard = self.panel.lock().map_err(|_| "panel lock poisoned")?;
            if let Some(panel) = guard.as_ref() {
                if panel.session == request.session && panel.bootstrap == request.bootstrap {
                    crate::panel_bridge::set_endpoint(request.endpoint.clone());
                    panel
                        .webview
                        .set_position(LogicalPosition::new(request.x, request.y))
                        .map_err(|error| error.to_string())?;
                    panel
                        .webview
                        .set_size(LogicalSize::new(request.width, request.height))
                        .map_err(|error| error.to_string())?;
                    panel.webview.show().map_err(|error| error.to_string())?;
                    if panel.webview.url().map(|current| current.as_str() != url.as_str()).unwrap_or(true) {
                        panel
                            .webview
                            .navigate(url)
                            .map_err(|error| error.to_string())?;
                    }
                    return Ok(());
                }
            }
        }

        // A different session replaces the panel outright.
        self.close_current()?;
        let window = app
            .get_window("main")
            .ok_or_else(|| "the main window is not available".to_string())?;
        let label = panel_label(&request.session);
        let data_dir = Self::data_dir(app)?;
        let mut builder = WebviewBuilder::new(label.clone(), WebviewUrl::External(url))
            .data_directory(data_dir)
            .devtools(true);
        if !request.bootstrap.is_empty() {
            builder = builder.initialization_script(request.bootstrap.clone());
        }
        let webview = window
            .add_child(
                builder,
                LogicalPosition::new(request.x, request.y),
                LogicalSize::new(request.width, request.height),
            )
            .map_err(|error| format!("cannot create the browser panel: {error}"))?;
        // The relay target and the script message handler are in place before the
        // page can get far: a secure page cannot POST to our loopback endpoint
        // itself, so this handler is its only page-to-host channel.
        crate::panel_bridge::set_endpoint(request.endpoint.clone());
        if let Err(error) = crate::panel_bridge::attach(&webview) {
            eprintln!("native browser panel: {error}");
        }
        let mut guard = self.panel.lock().map_err(|_| "panel lock poisoned")?;
        *guard = Some(Panel {
            session: request.session.clone(),
            endpoint: request.endpoint.clone(),
            bootstrap: request.bootstrap.clone(),
            webview,
        });
        Ok(())
    }

    /// Move, resize, or hide the panel.
    pub fn bounds(&self, request: &BoundsRequest) -> Result<(), String> {
        let guard = self.panel.lock().map_err(|_| "panel lock poisoned")?;
        let panel = guard.as_ref().ok_or_else(|| "no browser panel is open".to_string())?;
        if request.visible == Some(false) {
            return panel.webview.hide().map_err(|error| error.to_string());
        }
        panel
            .webview
            .set_position(LogicalPosition::new(request.x, request.y))
            .map_err(|error| error.to_string())?;
        panel
            .webview
            .set_size(LogicalSize::new(request.width, request.height))
            .map_err(|error| error.to_string())?;
        panel.webview.show().map_err(|error| error.to_string())
    }

    /// Run one navigation or script command against the panel.
    pub fn command(&self, request: &CommandRequest) -> Result<(), String> {
        match request.kind.as_str() {
            "close" => return self.close_current(),
            _ => {}
        }
        let guard = self.panel.lock().map_err(|_| "panel lock poisoned")?;
        let panel = guard.as_ref().ok_or_else(|| "no browser panel is open".to_string())?;
        let webview = &panel.webview;
        match request.kind.as_str() {
            "reload" => webview.reload().map_err(|error| error.to_string()),
            "navigate" => {
                let url: url::Url = request
                    .url
                    .as_deref()
                    .ok_or_else(|| "navigate needs a url".to_string())?
                    .parse()
                    .map_err(|error| format!("invalid url: {error}"))?;
                webview.navigate(url).map_err(|error| error.to_string())
            }
            "eval" => {
                let script = request
                    .script
                    .as_deref()
                    .ok_or_else(|| "eval needs a script".to_string())?;
                webview.eval(script).map_err(|error| error.to_string())
            }
            other => Err(format!("unsupported panel command: {other}")),
        }
    }

    /**
     * Capture the panel as a PNG at the panel's own device resolution.
     *
     * Unlike an in-page canvas capture this is the compositor's own bitmap, so
     * it is never tainted by cross-Origin images and needs no page cooperation.
     */
    pub fn snapshot(&self) -> Result<String, String> {
        let guard = self.panel.lock().map_err(|_| "panel lock poisoned")?;
        let panel = guard.as_ref().ok_or_else(|| "no browser panel is open".to_string())?;
        let (sender, receiver) = std::sync::mpsc::channel::<Result<Vec<u8>, String>>();
        let sender = std::sync::Mutex::new(sender);
        panel
            .webview
            .with_webview(move |platform| {
                #[cfg(target_os = "macos")]
                unsafe {
                    use block2::RcBlock;
                    use objc2_app_kit::NSImage;
                    use objc2_foundation::NSError;
                    use objc2_web_kit::WKWebView;

                    let view: &WKWebView = &*(platform.inner() as *mut WKWebView);
                    let completion = RcBlock::new(move |image: *mut NSImage, error: *mut NSError| {
                        let captured = snapshot_png(image, error);
                        if let Ok(guard) = sender.lock() {
                            let _ = guard.send(captured);
                        }
                    });
                    view.takeSnapshotWithConfiguration_completionHandler(None, &completion);
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = platform;
                    if let Ok(guard) = sender.lock() {
                        let _ = guard.send(Err("panel snapshots need macOS".to_string()));
                    }
                }
            })
            .map_err(|error| format!("cannot reach the panel: {error}"))?;
        match receiver.recv_timeout(std::time::Duration::from_secs(20)) {
            Ok(Ok(bytes)) => Ok(base64::engine::general_purpose::STANDARD.encode(bytes)),
            Ok(Err(message)) => Err(message),
            Err(_) => Err("panel snapshot timed out".to_string()),
        }
    }

    /// Report what the panel currently shows.
    pub fn state(&self) -> PanelState {
        let Ok(guard) = self.panel.lock() else {
            return PanelState { open: false, session: None, url: None, bounds: None, relay: false };
        };
        match guard.as_ref() {
            Some(panel) => {
                let scale = panel
                    .webview
                    .window()
                    .scale_factor()
                    .unwrap_or(1.0);
                let bounds = panel.webview.bounds().ok().map(|rect| {
                    let position = rect.position.to_logical::<f64>(scale);
                    let size = rect.size.to_logical::<f64>(scale);
                    PanelBounds {
                        x: position.x,
                        y: position.y,
                        width: size.width,
                        height: size.height,
                    }
                });
                PanelState {
                    open: true,
                    session: Some(panel.session.clone()),
                    url: panel.webview.url().ok().map(|url| url.to_string()),
                    bounds,
                    relay: panel.endpoint.is_some(),
                }
            }
            None => PanelState { open: false, session: None, url: None, bounds: None, relay: false },
        }
    }
}

/** Encode one `takeSnapshot` completion as PNG bytes. */
#[cfg(target_os = "macos")]
unsafe fn snapshot_png(image: *mut objc2_app_kit::NSImage, error: *mut objc2_foundation::NSError) -> Result<Vec<u8>, String> {
    use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep};
    use objc2_foundation::NSDictionary;
    use std::ptr::NonNull;

    if !error.is_null() {
        let error = &*error;
        return Err(format!("panel snapshot failed: {}", error.localizedDescription()));
    }
    let Some(image) = image.as_ref() else {
        return Err("panel snapshot returned no image".to_string());
    };
    let tiff = image
        .TIFFRepresentation()
        .ok_or_else(|| "panel snapshot has no bitmap representation".to_string())?;
    let rep = NSBitmapImageRep::imageRepWithData(&tiff)
        .ok_or_else(|| "panel snapshot is not decodable".to_string())?;
    let png = rep
        .representationUsingType_properties(NSBitmapImageFileType::PNG, &NSDictionary::new())
        .ok_or_else(|| "panel snapshot could not be encoded as PNG".to_string())?;
    let length = png.length();
    let mut buffer = vec![0u8; length];
    let pointer = NonNull::new(buffer.as_mut_ptr().cast()).ok_or_else(|| "empty snapshot".to_string())?;
    png.getBytes_length(pointer, length);
    Ok(buffer)
}
