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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

/// Script message handler name; the bootstrap posts to exactly this handler.
///
/// The name is a WebKit concept only: WebView2 has a single, unnamed message
/// channel per webview, so the Windows side installs it without a name.
#[cfg(target_os = "macos")]
pub const HANDLER_NAME: &str = "dshWebReview";

/// Longest single payload the relay forwards.
const MAX_PAYLOAD_BYTES: usize = 4 * 1024 * 1024;

/// Where the current panel's page messages are relayed.
static ENDPOINT: Mutex<Option<String>> = Mutex::new(None);

/// Whether the current panel actually carries the script message handler.
///
/// A relay target alone proves nothing: the shell must also have installed the
/// handler the page posts to, or the panel is back to being unreachable from a
/// secure document.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Whether the panel can receive page messages (diagnostic for `/panel/state`).
pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// Point the relay at the loopback endpoint the current panel reports to.
pub fn set_endpoint(endpoint: Option<String>) {
    match ENDPOINT.lock() {
        Ok(mut guard) => *guard = endpoint,
        Err(_) => {}
    }
    ARMED.store(false, Ordering::Relaxed);
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
        .map_err(|error| format!("cannot reach the panel to install its message handler: {error}"))?;
    ARMED.store(true, Ordering::Relaxed);
    Ok(())
}

/// Install the WebView2 message handler on a freshly created panel webview.
///
/// Windows has no `WKScriptMessageHandler`; the equivalent is WebView2's own
/// `WebMessageReceived`, which the page reaches through
/// `window.chrome.webview.postMessage(...)`. The contract is the same as the
/// AppKit side: the payload is reposted from this process, where the browser's
/// transport rules (mixed content, Private Network Access) do not apply.
#[cfg(windows)]
pub fn attach(webview: &tauri::Webview<tauri::Wry>) -> Result<(), String> {
    use webview2_com::Microsoft::Web::WebView2::Win32::{
        ICoreWebView2, ICoreWebView2WebMessageReceivedEventArgs,
    };
    use webview2_com::WebMessageReceivedEventHandler;
    use windows::core::PWSTR;

    webview
        .with_webview(|platform| {
            let core = match unsafe { platform.controller().CoreWebView2() } {
                Ok(core) => core,
                Err(error) => {
                    eprintln!("native browser panel: cannot reach the WebView2 core: {error}");
                    return;
                }
            };
            let handler = WebMessageReceivedEventHandler::create(Box::new(
                move |_sender: Option<ICoreWebView2>,
                      args: Option<ICoreWebView2WebMessageReceivedEventArgs>| {
                    let Some(args) = args else { return Ok(()) };
                    let mut message = PWSTR::null();
                    if unsafe { args.TryGetWebMessageAsString(&mut message) }.is_ok() {
                        if let Ok(text) = unsafe { message.to_string() } {
                            relay(text);
                        }
                    }
                    Ok(())
                },
            ));
            // The token is only needed to unregister later; this handler lives
            // as long as the panel webview does.
            let mut token = 0i64;
            match unsafe { core.add_WebMessageReceived(&handler, &mut token) } {
                Ok(()) => ARMED.store(true, Ordering::Relaxed),
                Err(error) => {
                    eprintln!("native browser panel: cannot install the message handler: {error}")
                }
            }
        })
        .map_err(|error| format!("cannot reach the panel to install its message handler: {error}"))?;
    Ok(())
}

/// Other shells have no native message channel to install.
#[cfg(not(any(target_os = "macos", windows)))]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// The relay must write one well-formed POST carrying the page payload.
    #[test]
    fn posts_the_payload_to_the_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let endpoint = format!("http://127.0.0.1:{port}/native-event?sessionId=abc&channel=def");

        let accepted = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().expect("accept");
            let mut buffer = [0u8; 1024];
            let read = socket.read(&mut buffer).expect("read");
            String::from_utf8_lossy(&buffer[..read]).to_string()
        });

        post(&endpoint, br#"{"type":"state"}"#).expect("relay");
        let request = accepted.join().expect("join");
        assert!(request.starts_with("POST /native-event?sessionId=abc&channel=def HTTP/1.1\r\n"), "{request}");
        assert!(request.contains("Content-Length: 16\r\n"), "{request}");
        assert!(request.ends_with(r#"{"type":"state"}"#), "{request}");
    }

    /// Only a loopback endpoint may be relayed to, and only over http.
    #[test]
    fn refuses_a_foreign_endpoint() {
        assert!(post("http://example.com/native-event", b"{}").is_err());
        assert!(post("https://127.0.0.1:1/native-event", b"{}").is_err());
    }

    /// The arming flag follows the endpoint the panel was opened with.
    #[test]
    fn arming_tracks_the_panel() {
        set_endpoint(None);
        assert!(!armed());
        set_endpoint(Some("http://127.0.0.1:1/native-event".to_string()));
        assert!(!armed(), "a target alone is not an installed handler");
        set_endpoint(None);
        assert!(!armed());
    }
}
