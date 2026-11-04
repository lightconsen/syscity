package net.syscity.desktop

/**
 * The native side of the device event stream.
 *
 * This is the one place Kotlin pushes into Rust instead of being asked. The
 * accessibility service calls [onDeviceEvent] with an already-serialized event
 * (`DeviceEvent.toJson()`); the Rust export appends it to a broadcast channel
 * that a waiting tool is subscribed to. Nothing here reads state, returns a
 * value, or can block — a JNI call that had to call back into Kotlin would put
 * the callback thread behind whatever the main thread is doing.
 *
 * Two things about this declaration are not stylistic:
 *
 * 1. **It is a separate object, not a method on `Rust`.** `Rust.kt` is
 *    auto-generated (`DO NOT MODIFY`), and the JNI symbol name follows the
 *    declaring class: `Java_net_syscity_desktop_NativeEvents_onDeviceEvent`.
 *    It is declared nowhere else.
 * 2. **It must not be `internal`.** Kotlin mangles internal function names with
 *    a `$module` suffix, and the exported symbol would stop matching.
 *
 * `loadLibrary` lives in this object's own initializer so that the first call
 * to [onDeviceEvent] is also what loads the library — the call does not depend
 * on `Rust` having been touched already. The class initializer runs on whatever
 * thread makes the first call; `DeviceEvents` therefore wraps the call in a
 * `Throwable` catch, because a missing or ABI-mismatched library fails with an
 * `UnsatisfiedLinkError`, which would otherwise kill that thread.
 *
 * The symbol is only resolved at runtime, so a typo here is invisible to the
 * compiler and to every test that does not run on a device. `proguard-rules.pro`
 * keeps this class intact for release builds (defensive: the default rules
 * already keep classes with native methods).
 */
object NativeEvents {
  init {
    System.loadLibrary("syscity_desktop_lib")
  }

  @JvmStatic external fun onDeviceEvent(json: String)
}
