//! Window key guard: ignore Escape while the window is fullscreen.
//!
//! macOS offers no supported way to keep a window fullscreen when Escape is
//! pressed, and the webview also treats Escape as "leave fullscreen". A local
//! event monitor runs before either of them sees the key, so this guard can
//! consume it — but only while the window is actually fullscreen, because
//! Escape still has to reach the page (the preview picker uses it to cancel a
//! selection) in a normal window.

use std::sync::atomic::{AtomicU64, Ordering};

/// Escape's virtual key code on macOS.
#[cfg(target_os = "macos")]
const ESCAPE_KEY_CODE: u16 = 53;

static ESCAPES_SWALLOWED: AtomicU64 = AtomicU64::new(0);

/// How many Escape presses the guard has consumed so far (a diagnostic).
pub fn escapes_swallowed() -> u64 {
    ESCAPES_SWALLOWED.load(Ordering::Relaxed)
}

/// Consume Escape while `window` is fullscreen, for the life of the process.
#[cfg(target_os = "macos")]
pub fn install(window: tauri::Window) {
    use block2::RcBlock;
    use objc2_app_kit::{NSEvent, NSEventMask};
    use std::ptr::NonNull;

    let guard = window.clone();
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        let key_code = unsafe { event.as_ref().keyCode() };
        if key_code == ESCAPE_KEY_CODE && guard.is_fullscreen().unwrap_or(false) {
            ESCAPES_SWALLOWED.fetch_add(1, Ordering::Relaxed);
            // Null means "consumed": neither the window nor the webview sees it.
            return std::ptr::null_mut();
        }
        event.as_ptr()
    });
    let token = unsafe {
        NSEvent::addLocalMonitorForEventsMatchingMask_handler(NSEventMask::KeyDown, &block)
    };
    // The monitor is meant to outlive setup; keep its token rather than dropping
    // it, which would let the block be released while AppKit still calls it.
    std::mem::forget(token);
}

/// Non-macOS shells have no AppKit event monitor to install.
#[cfg(not(target_os = "macos"))]
pub fn install(_window: tauri::Window) {}
