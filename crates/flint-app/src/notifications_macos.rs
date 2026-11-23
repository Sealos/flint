//! Posts through `UNUserNotificationCenter` and reports what macOS answers.
//!
//! GPUI's own call discards the answer, so posting is done here. GPUI still owns the center's
//! delegate, which routes a click on the notification back by its identifier.
//!
//! The center aborts the process outside an app bundle, so every path checks for one first.

// module-tests: notifications.rs (the macOS answers need a signed bundle; see the error mapping)

use std::ptr::NonNull;
use std::sync::mpsc;
use std::time::Duration;

use block2::RcBlock;
use objc2::runtime::Bool;
use objc2_foundation::{NSBundle, NSError, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent,
    UNNotificationRequest, UNNotificationSetting, UNNotificationSettings, UNUserNotificationCenter,
};

use super::{DeliveryError, Notification, NotificationSender};

/// How long macOS may take to answer a status or post. The permission prompt has its own limit.
const ANSWER_WAIT: Duration = Duration::from_secs(5);

/// How long the person may take to answer the permission prompt.
const PROMPT_WAIT: Duration = Duration::from_secs(120);

/// Sends through the macOS notification center.
#[derive(Clone, Copy, Debug, Default)]
pub struct MacosSender;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Permission {
    Granted,
    NotAsked,
    Refused,
}

fn center() -> Result<objc2::rc::Retained<UNUserNotificationCenter>, DeliveryError> {
    if NSBundle::mainBundle().bundleIdentifier().is_none() {
        return Err(DeliveryError::NeedsAppBundle);
    }
    Ok(UNUserNotificationCenter::currentNotificationCenter())
}

fn no_answer() -> DeliveryError {
    DeliveryError::Failed("macOS did not answer".to_owned())
}

fn permission(center: &UNUserNotificationCenter) -> Result<Permission, DeliveryError> {
    let (answer, answered) = mpsc::channel();
    let completion = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        // SAFETY: macOS passes a valid settings object for the duration of the callback.
        let settings = unsafe { settings.as_ref() };
        let status = settings.authorizationStatus();
        let visible = settings.alertSetting() == UNNotificationSetting::Enabled
            || settings.notificationCenterSetting() == UNNotificationSetting::Enabled;
        let _ = answer.send(if status == UNAuthorizationStatus::NotDetermined {
            Permission::NotAsked
        } else if visible
            && (status == UNAuthorizationStatus::Authorized
                || status == UNAuthorizationStatus::Provisional)
        {
            Permission::Granted
        } else {
            Permission::Refused
        });
    });
    center.getNotificationSettingsWithCompletionHandler(&completion);
    answered.recv_timeout(ANSWER_WAIT).map_err(|_| no_answer())
}

fn error_text(error: *mut NSError) -> Option<String> {
    // SAFETY: when non-null, `error` is a valid `NSError` for the duration of the callback.
    unsafe { error.as_ref() }.map(|error| error.localizedDescription().to_string())
}

impl NotificationSender for MacosSender {
    fn prepare(&self) -> Result<(), DeliveryError> {
        let center = center()?;
        match permission(&center)? {
            Permission::Granted => return Ok(()),
            Permission::Refused => return Err(DeliveryError::NotPermitted),
            Permission::NotAsked => {}
        }
        let (answer, answered) = mpsc::channel();
        let completion = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            let _ = answer.send((granted.as_bool(), error_text(error)));
        });
        center.requestAuthorizationWithOptions_completionHandler(
            UNAuthorizationOptions::Alert | UNAuthorizationOptions::Sound,
            &completion,
        );
        match answered.recv_timeout(PROMPT_WAIT) {
            Ok((true, _)) => Ok(()),
            Ok((false, None)) | Err(_) => Err(DeliveryError::NotPermitted),
            Ok((false, Some(why))) => {
                tracing::warn!("notification permission: {why}");
                Err(DeliveryError::NotPermitted)
            }
        }
    }

    fn send(&self, notification: &Notification) -> Result<(), DeliveryError> {
        let center = center()?;
        // Never prompts here: only turning notifications on asks.
        if permission(&center)? != Permission::Granted {
            return Err(DeliveryError::NotPermitted);
        }
        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(&notification.title));
        content.setBody(&NSString::from_str(&notification.body));
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(&notification.tag),
            &content,
            None,
        );
        let (answer, answered) = mpsc::channel();
        let completion = RcBlock::new(move |error: *mut NSError| {
            let _ = answer.send(error_text(error));
        });
        center.addNotificationRequest_withCompletionHandler(&request, Some(&completion));
        match answered.recv_timeout(ANSWER_WAIT) {
            Ok(None) => Ok(()),
            Ok(Some(why)) => Err(DeliveryError::Failed(why)),
            Err(_) => Err(no_answer()),
        }
    }
}
