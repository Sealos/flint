//! Brings the app's window to the front on macOS, for tests that need an active window.
//!
//! From macOS 14 the system may refuse an app's request to activate itself (cooperative
//! activation), and GPUI's `active_window` is AppKit's main window, which an inactive app may not
//! have. So this asks in every way AppKit offers, and reports what AppKit says afterwards, so a
//! test that still sees no active window can say why.

// module-tests: protocol.rs (the command parses there; AppKit's answers need a real GUI session,
// so `window_close.rs` in flint-e2e exercises this)

use objc2::runtime::{AnyObject, Bool};
use objc2::{class, msg_send};
use serde_json::{Value, json};

/// `NSApplicationActivationPolicyRegular`.
const POLICY_REGULAR: isize = 0;
/// `NSApplicationActivateIgnoringOtherApps`.
const IGNORING_OTHER_APPS: usize = 1 << 1;

/// Makes the app regular and active and its first window main and key. Must run on the main
/// thread. Answers what AppKit reports once the requests are made.
pub fn activate() -> Value {
    // SAFETY: plain AppKit messages to the shared application, its windows and the running
    // application, on the main thread, with the types AppKit documents.
    unsafe {
        let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
        let policy_before: isize = msg_send![app, activationPolicy];
        let policy_set: Bool = msg_send![app, setActivationPolicy: POLICY_REGULAR];
        let _: () = msg_send![app, unhide: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![app, activateIgnoringOtherApps: Bool::YES];
        let running: *mut AnyObject = msg_send![class!(NSRunningApplication), currentApplication];
        let accepted: Bool = msg_send![running, activateWithOptions: IGNORING_OTHER_APPS];
        let windows: *mut AnyObject = msg_send![app, windows];
        let count: usize = msg_send![windows, count];
        if count > 0 {
            let window: *mut AnyObject = msg_send![windows, objectAtIndex: 0usize];
            let _: () = msg_send![window, makeMainWindow];
            let _: () = msg_send![window, makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()];
        }
        let main: *mut AnyObject = msg_send![app, mainWindow];
        let key: *mut AnyObject = msg_send![app, keyWindow];
        let active: Bool = msg_send![app, isActive];
        let front: Bool = msg_send![running, isActive];
        json!({
            "policy_before": policy_before,
            "policy_set": policy_set.as_bool(),
            "activate_accepted": accepted.as_bool(),
            "app_active": active.as_bool(),
            "running_active": front.as_bool(),
            "windows": count,
            "has_main_window": !main.is_null(),
            "has_key_window": !key.is_null(),
        })
    }
}
