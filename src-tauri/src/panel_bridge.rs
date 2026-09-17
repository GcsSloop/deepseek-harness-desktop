//! Page-to-host message relay for the native browser panel.
//!
//! A previewed **HTTPS** page cannot reach the plugin's loopback HTTP endpoint:
//! WebKit refuses an insecure request from a secure document. Measured in a live
//! panel (`https://cyberhub.sansi.net:12443/...`): `fetch('http://127.0.0.1:…')`
//! rejects with "Load failed", `sendBeacon` returns `false`, and the same is true
//! for `localhost` — while an HTTP page reports fine. Without this relay the
//! panel renders but never handshakes, so the picker never arms.
//!
//! The supported channel is the webview's own script message handler, which the
//! injected bootstrap reaches through
//! `window.webkit.messageHandlers.<name>.postMessage(...)`. This module owns that
//! handler, remembers the relay target the plugin named when it opened the panel,
//! and reposts each payload from this process — where the browser's transport
//! rules do not apply.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Mutex;
use std::time::Duration;

/// Script message handler name; the bootstrap posts to exactly this handler.
pub const HANDLER_NAME: &str = "dshWebReview";

/// Longest single payload the relay forwards.
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// Where the current panel's page messages are relayed.
static ENDPOINT: Mutex<Option<String>> = Mutex::new(None);

/// Point the relay at the loopback endpoint the current panel reports to.
pub fn set_endpoint(endpoint: Option<String>) {
    match ENDPOINT.lock() {
        Ok(mut guard) => *guard = endpoint,
        Err(_) => {}
    }
}

/// Relay one page payload to the plugin endpoint, off the main thread.
pub fn relay(payload: String) {
    if payload.is_empty() || payload.len() > MAX_PAYLOAD_BYTES {
        return;
    }
    let endpoint = match ENDPOINT.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    };
    let Some(endpoint) = endpoint else {
        return;
    };
    // A page message must never block AppKit's main thread.
    std::thread::spawn(move || {
        let _ = post(&endpoint, payload.as_bytes());
    });
}

/// One minimal HTTP/1.1 POST; the endpoint answers 204 and the reply is dropped.
fn post(endpoint: &str, body: &[u8]) -> Result<(), String> {
    let rest = endpoint
        .strip_prefix("http://")
        .ok_or_else(|| "relay endpoint must be loopback http".to_string())?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    if !authority.starts_with("127.0.0.1:") {
        return Err("relay endpoint must be loopback".to_string());
    }
    let mut stream = TcpStream::connect(authority).map_err(|error| error.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| error.to_string())?;
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {authority}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).map_err(|error| error.to_string())?;
    stream.write_all(body).map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())?;
    // Read (and discard) the acknowledgement so the endpoint is not reset.
    let mut sink = [0u8; 256];
    let _ = stream.read(&mut sink);
    Ok(())
}

/// Install the script message handler on a freshly created panel webview.
#[cfg(target_os = "macos")]
pub fn attach(webview: &tauri::Webview<tauri::Wry>) -> Result<(), String> {
    use objc2::runtime::ProtocolObject;
    use objc2_foundation::ns_string;
    use objc2_web_kit::WKWebView;

    webview
        .with_webview(|platform| {
            let view: &WKWebView = unsafe { &*(platform.inner() as *mut WKWebView) };
            let controller = unsafe { view.configuration().userContentController() };
            let handler = ScriptMessageHandler::new();
            unsafe {
                controller.addScriptMessageHandler_name(
                    ProtocolObject::from_ref(&*handler),
                    ns_string!(HANDLER_NAME),
                );
            }
            // The controller retains its handler; the local handle may drop.
            std::mem::drop(handler);
        })
        .map_err(|error| format!("cannot reach the panel to install its message handler: {error}"))
}

/// Non-macOS shells have no WKWebView to install a handler on.
#[cfg(not(target_os = "macos"))]
pub fn attach(_webview: &tauri::Webview<tauri::Wry>) -> Result<(), String> {
    Ok(())
}

/// The `WKScriptMessageHandler` that turns page messages into relayed payloads.
#[cfg(target_os = "macos")]
mod handler {
    use objc2::rc::Retained;
    use objc2::runtime::NSObjectProtocol;
    use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
    use objc2_foundation::{NSObject, NSString};
    use objc2_web_kit::{WKScriptMessage, WKScriptMessageHandler, WKUserContentController};

    // No ivars: the relay target is per panel and lives in the module static.
    define_class!(
        // SAFETY: NSObject imposes no subclassing requirements and this class
        // does not implement Drop.
        #[unsafe(super(NSObject))]
        // WebKit calls the handler on the main thread only.
        #[thread_kind = MainThreadOnly]
        #[name = "DshWebReviewScriptMessageHandler"]
        pub struct ScriptMessageHandler;

        unsafe impl NSObjectProtocol for ScriptMessageHandler {}

        unsafe impl WKScriptMessageHandler for ScriptMessageHandler {
            #[unsafe(method(userContentController:didReceiveScriptMessage:))]
            fn did_receive(
                &self,
                _controller: &WKUserContentController,
                message: &WKScriptMessage,
            ) {
                let body = unsafe { message.body() };
                // Only a string payload is one of our bridge messages.
                let Some(text) = body.downcast_ref::<NSString>() else {
                    return;
                };
                super::relay(text.to_string());
            }
        }
    );

    impl ScriptMessageHandler {
        /// Create one handler; the user content controller owns it afterwards.
        ///
        /// # Panics
        /// When called off the main thread: WebKit invokes this handler there.
        pub fn new() -> Retained<Self> {
            let marker = MainThreadMarker::new().expect("panel handlers are main-thread only");
            let this = Self::alloc(marker).set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }
}

#[cfg(target_os = "macos")]
use handler::ScriptMessageHandler;
