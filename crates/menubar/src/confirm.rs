//! Native dialogs for menu actions: a yes/no question before a destructive one and a one-button result after.
//!
//! The dialog is an `NSAlert` run modally on the main thread, so no text goes through a shell or AppleScript.

use objc2::MainThreadMarker;
use objc2_app_kit::{NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication};
use objc2_foundation::{NSOperatingSystemVersion, NSString};

/// The app has no Dock icon and no window, so it must come to the front for a dialog to be seen.
fn bring_to_front(mtm: MainThreadMarker) {
    let app = NSApplication::sharedApplication(mtm);
    if objc2_foundation::NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(NSOperatingSystemVersion { majorVersion: 14, minorVersion: 0, patchVersion: 0 }) {
        app.activate();
    } else {
        #[allow(deprecated)]
        app.activateIgnoringOtherApps(true);
    }
}

/// Show `message` and `detail` with a `confirm` button and a Cancel button, and return whether the user chose
/// `confirm`. Cancel is the default button, so Return and Escape both cancel. Off the main thread nothing is
/// shown and the answer is no.
pub fn ask(message: &str, detail: &str, confirm: &str) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    bring_to_front(mtm);
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(message));
    alert.setInformativeText(&NSString::from_str(detail));
    // The first button added is the default one.
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.addButtonWithTitle(&NSString::from_str(confirm));
    alert.runModal() == NSAlertFirstButtonReturn + 1
}

/// Show `message` and `detail` with one OK button and wait for it. Off the main thread nothing is shown.
pub fn inform(message: &str, detail: &str) {
    let Some(mtm) = MainThreadMarker::new() else { return };
    bring_to_front(mtm);
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);
    alert.setMessageText(&NSString::from_str(message));
    alert.setInformativeText(&NSString::from_str(detail));
    alert.addButtonWithTitle(&NSString::from_str("OK"));
    alert.runModal();
}
