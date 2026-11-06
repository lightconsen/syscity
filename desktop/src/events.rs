//! The JNI entry points that let the phone push into Rust.
//!
//! Everything else in the bridge is request/response: Rust calls a Kotlin
//! `@Command` and waits. Events are the opposite direction — the accessibility
//! service is woken by the system and has to wake us — so the phone calls the
//! exported symbol below directly, which appends to a `broadcast` channel a
//! waiting tool is subscribed to.
//!
//! Four constraints shape this file, and none of them is stylistic:
//!
//! 1. **Never panic across the FFI boundary.** `extern "system"` is a
//!    non-unwinding ABI; a panic that crosses it aborts the whole process.
//!    Every body is wrapped in [`std::panic::catch_unwind`].
//! 2. **No blocking, no `.await`, no I/O.** The callback can arrive on any
//!    thread — on Android it is the accessibility service's serial executor —
//!    so the body parses the string and sends it. Nothing else.
//! 3. **The sender is created before the first event, not by it.** A `OnceLock`
//!    that a callback initializes is a race the callback loses: events posted
//!    while the app is starting would find no channel. [`sender`] is created
//!    from the plugin's setup path (see `mobile_device::device_plugin`), and a
//!    callback that runs before that simply drops its event — the ring in
//!    Kotlin still has it.
//! 4. **Zero subscribers is fine.** `broadcast::Sender::send` fails when
//!    nobody is listening, and that error is dropped on purpose: an event
//!    nobody was waiting for is exactly the history the ring is for.
//!
//! The signal name is decided by the *declaring* Kotlin class, not by the
//! generated `Rust` object: `net.syscity.desktop.NativeEvents` declares a
//! `@JvmStatic external fun onDeviceEvent(json: String)`, so the symbol is
//! `Java_net_syscity_desktop_NativeEvents_onDeviceEvent`. A typo in either
//! half compiles cleanly and fails at runtime with `UnsatisfiedLinkError`.

// Everything here is live only where the phone can push: on a desktop host the
// channel has no producer, and `TauriDeviceBridge::events` is compiled out of
// the Android branch. The code still has to exist on every target so it can be
// tested without a device, so the "unused" warning is silenced rather than the
// code removed.
#![cfg_attr(not(target_os = "android"), allow(dead_code))]

/// The process-wide event channel.
///
/// Created by [`init`] during plugin setup; read by [`sender`] from the JNI
/// callback and by `TauriDeviceBridge::events`. The capacity bounds what a
/// slow subscriber can fall behind by; past it the *subscriber* starts missing
/// events and gets a `RecvError::Lagged`, which is reported rather than hidden.
static CHANNEL: std::sync::OnceLock<tokio::sync::broadcast::Sender<syscity::device::DeviceEvent>> =
    std::sync::OnceLock::new();

/// Buffer depth for the push channel.
///
/// A waiter that is already awake drains this in microseconds; the depth only
/// matters for a burst (a screen repainting) or a parked subscriber. Lag past
/// this is reported to the subscriber, not silently trimmed.
const CHANNEL_CAPACITY: usize = 256;

/// Create the channel. Idempotent — the first call wins.
///
/// Called from the device plugin's setup, before the gateway can ask for a
/// subscription. It is safe to call more than once (iOS and Android both reach
/// this path, and a re-registration is possible).
pub(crate) fn init() {
    let _ = CHANNEL.set(tokio::sync::broadcast::channel(CHANNEL_CAPACITY).0);
}

/// The channel's sender, or `None` before [`init`].
pub(crate) fn sender(
) -> Option<&'static tokio::sync::broadcast::Sender<syscity::device::DeviceEvent>> {
    CHANNEL.get()
}

/// Publish one event. Returns how many subscribers received it (0 is normal).
///
/// Kept out of the `extern "system"` body so it can be tested on a host; the
/// JNI shim is then only the string extraction and the panic guard.
pub(crate) fn publish(json: &str) -> usize {
    match sender() {
        // Before setup there is no channel. The Kotlin ring still has the
        // event, so nothing is lost that a reader cannot fetch.
        Some(sender) => publish_to(sender, json),
        None => 0,
    }
}

/// [`publish`] into a given channel, so the rules are testable without the
/// process-wide one (which every other test in the same binary also uses).
pub(crate) fn publish_to(
    sender: &tokio::sync::broadcast::Sender<syscity::device::DeviceEvent>,
    json: &str,
) -> usize {
    let Some(event) = syscity::device::DeviceEvent::from_json(json) else {
        // An unknown kind or malformed JSON: dropping it is better than
        // reporting a fact this build cannot describe.
        return 0;
    };
    // `send` fails when nobody is subscribed. That is not a failure: an event
    // nobody was waiting for is served by the ring instead.
    sender.send(event).unwrap_or(0)
}

#[cfg(target_os = "android")]
mod android {
    use super::publish;
    use jni::objects::{JClass, JString};
    use jni::JNIEnv;

    /// `NativeEvents.onDeviceEvent(json)` — the phone pushing one event.
    ///
    /// # Safety
    ///
    /// Called by the JVM with a valid `JNIEnv` for the current thread, the
    /// `NativeEvents` class, and a non-null `String`. Those are the JNI
    /// calling conventions; nothing here trusts anything else about them.
    ///
    /// The whole body is panic-guarded: `extern "system"` does not unwind, and
    /// an escaping panic would abort Syscity itself rather than fail one event.
    #[no_mangle]
    pub extern "system" fn Java_net_syscity_desktop_NativeEvents_onDeviceEvent(
        mut env: JNIEnv,
        _class: JClass,
        payload: JString,
    ) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            // A `get_string` failure means the JVM refused to hand over the
            // text — nothing to publish, and throwing back into Kotlin would
            // just kill the executor thread that called us.
            let Ok(json) = env.get_string(&payload) else {
                return;
            };
            let json: String = json.into();
            publish(&json);
        }));
        if result.is_err() {
            // Contained, and deliberately not re-raised. `tracing` is a no-op
            // without a subscriber, so this cannot panic a second time.
            tracing::error!("device event callback panicked; the event was dropped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A channel of this test's own.
    ///
    /// The real one is process-wide and shared with every other test in this
    /// binary running in parallel — using it here would make the subscriber
    /// count depend on what else is running, which is exactly the kind of test
    /// that passes until it doesn't.
    fn channel() -> tokio::sync::broadcast::Sender<syscity::device::DeviceEvent> {
        tokio::sync::broadcast::channel(8).0
    }

    #[test]
    fn a_published_event_reaches_a_subscriber() {
        let sender = channel();
        let mut receiver = sender.subscribe();
        let received = publish_to(
            &sender,
            r#"{"seq":1,"kind":"notification","package":"com.example","atUptimeMs":5,
                "title":"Alice","text":"Are you free?"}"#,
        );
        assert_eq!(received, 1);
        let event = receiver.try_recv().expect("the event");
        assert_eq!(event.seq, 1);
        assert_eq!(event.kind, syscity::device::EventKind::Notification);
        assert_eq!(event.package, "com.example");
        assert_eq!(event.title.as_deref(), Some("Alice"));
    }

    #[test]
    fn malformed_json_is_dropped_without_waking_anyone() {
        let sender = channel();
        let mut receiver = sender.subscribe();
        assert_eq!(publish_to(&sender, "not json"), 0);
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn an_unknown_kind_is_dropped_rather_than_mislabelled() {
        let sender = channel();
        let mut receiver = sender.subscribe();
        assert_eq!(
            publish_to(&sender, r#"{"seq":1,"kind":"toast","package":"x","atUptimeMs":1}"#),
            0
        );
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn nobody_listening_is_not_an_error() {
        // A phone event between two waits: the ring in Kotlin still has it, so
        // dropping it here loses nothing a reader cannot fetch.
        let sender = channel();
        assert_eq!(
            publish_to(&sender, r#"{"seq":9,"kind":"window","package":"x","atUptimeMs":1}"#),
            0
        );
    }

    #[test]
    fn every_subscriber_gets_the_same_event() {
        let sender = channel();
        let mut first = sender.subscribe();
        let mut second = sender.subscribe();
        assert_eq!(
            publish_to(&sender, r#"{"seq":3,"kind":"content","package":"x","atUptimeMs":1}"#),
            2
        );
        assert_eq!(first.try_recv().expect("first").seq, 3);
        assert_eq!(second.try_recv().expect("second").seq, 3);
    }

    #[test]
    fn publishing_before_the_channel_exists_drops_the_event() {
        // `publish` must not panic on the path where the plugin's setup has
        // not run yet — the phone can deliver an event during startup.
        assert_eq!(publish(r#"{"seq":1,"kind":"window","package":"x","atUptimeMs":1}"#), 0);
    }
}
