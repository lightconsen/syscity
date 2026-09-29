// Syscity native iOS device bridge (mobile-migration §4.4/§4.6).
//
// Registered from Rust via `register_ios_plugin(init_plugin_syscity_device)`
// in `desktop/src/lib.rs`; the Rust runtime reaches these methods through the
// `DeviceBridge` trait. Each `@objc` command name must match the `CMD_*`
// constants in `src/device/mod.rs` verbatim — Tauri dispatches the command
// string to the ObjC selector `<command>:` with no transformation.
//
// Commands run on Tauri's serial IPC background queue, so any UIKit work hops
// to the main thread and `invoke.resolve`/`reject` may be called later from a
// delegate callback (mirroring the Kotlin `DevicePlugin`).

import Foundation
import UIKit
import CoreLocation
import AVFoundation
import UserNotifications
import AudioToolbox
import ObjectiveC
import WebKit

import Tauri

class DevicePlugin: Plugin {

  // Tauri calls this when the WKWebView is created. The keyboard "Done" bar is
  // native WKWebView UI (the web side can't remove it), so swizzle it here.
  @objc override func load(webview: WKWebView) {
    super.load(webview: webview)
    hideWebViewInputAccessory()
  }

  private struct NotifyArgs: Decodable {
    var title: String?
    var body: String?
  }

  private struct VibrateArgs: Decodable {
    var duration_ms: Int?
  }

  // ── In-flight async state (single user at a time) ────────────────────
  private var cameraPicker: UIImagePickerController?
  private var pendingCameraInvoke: Invoke?

  private var documentPicker: UIDocumentPickerViewController?
  private var pendingPickerInvoke: Invoke?

  // Location flows: `authRequest` resolves `requestPermission`, `fixRequest`
  // resolves `getLocation`. Only one of each is in flight at a time.
  private var authRequest: (manager: CLLocationManager, invoke: Invoke)?
  private var fixRequest: (manager: CLLocationManager, invoke: Invoke)?
  private var fixTimer: Timer?

  // ── Shared helpers ───────────────────────────────────────────────────

  /// The syscity data directory. `main.mm` sets `SYSCITY_HOME` from
  /// NSDocumentDirectory before Rust starts; fall back to the same path so the
  /// plugin and the gateway always agree.
  private var syscityHome: String {
    if let home = ProcessInfo.processInfo.environment["SYSCITY_HOME"], !home.isEmpty {
      return home
    }
    let docs =
      NSSearchPathForDirectoriesInDomains(.documentDirectory, .userDomainMask, true).first
      ?? NSTemporaryDirectory()
    return (docs as NSString).appendingPathComponent("syscity")
  }

  private static func timestamp() -> String {
    let f = DateFormatter()
    f.dateFormat = "yyyyMMdd_HHmmss"
    return f.string(from: Date())
  }

  /// Strip any path separators / traversal before it becomes a filename.
  private static func sanitizeName(_ name: String) -> String {
    var s = name
    if s.contains("/") { s = String(s.split(separator: "/").last ?? "") }
    if s.contains("\\") { s = String(s.split(separator: "\\").last ?? "") }
    s = s.replacingOccurrences(of: "..", with: "_").trimmingCharacters(in: .whitespacesAndNewlines)
    return s.isEmpty ? "file" : s
  }

  /// Return a non-colliding path, appending `_1`, `_2`, … on conflicts.
  private static func uniqueTarget(dir: String, name: String) -> String {
    var target = (dir as NSString).appendingPathComponent(name)
    var counter = 1
    while FileManager.default.fileExists(atPath: target) {
      let dot = name.lastIndex(of: ".")
      let base = dot.map { String(name[name.startIndex..<$0]) } ?? name
      let ext = dot.map { String(name[$0...]) } ?? ""
      target = (dir as NSString).appendingPathComponent("\(base)_\(counter)\(ext)")
      counter += 1
    }
    return target
  }

  private func resolvePermission(_ invoke: Invoke, granted: Bool, state: String) {
    var out = JsonObject()
    out["granted"] = granted
    out["state"] = state
    invoke.resolve(JsonValue.dictionary(out))
  }

  private func resolveLocation(_ invoke: Invoke, _ loc: CLLocation) {
    var out = JsonObject()
    out["latitude"] = loc.coordinate.latitude
    out["longitude"] = loc.coordinate.longitude
    out["accuracy_meters"] = loc.horizontalAccuracy
    out["timestamp_ms"] = Int(loc.timestamp.timeIntervalSince1970 * 1000)
    invoke.resolve(JsonValue.dictionary(out))
  }

  // ── Permissions ──────────────────────────────────────────────────────

  /// `permissionStatus` — report the grant state of a runtime permission.
  @objc public func permissionStatus(_ invoke: Invoke) throws {
    guard let alias = try invoke.getArgs()["permission"] as? String else {
      invoke.reject("Missing 'permission'", code: "INVALID_ARGUMENTS")
      return
    }
    switch alias {
    case "haptics", "file_pick", "adb":
      // Permission-free capabilities report granted unconditionally.
      resolvePermission(invoke, granted: true, state: "GRANTED")
    case "camera":
      let st = AVCaptureDevice.authorizationStatus(for: .video)
      resolvePermission(invoke, granted: st == .authorized, state: permissionStateString(st == .authorized))
    case "location":
      let st = CLLocationManager.authorizationStatus()
      let granted = st == .authorizedWhenInUse || st == .authorizedAlways
      let state = st == .notDetermined ? "PROMPT" : (granted ? "GRANTED" : "DENIED")
      resolvePermission(invoke, granted: granted, state: state)
    case "notifications":
      UNUserNotificationCenter.current().getNotificationSettings { settings in
        let granted = settings.authorizationStatus == .authorized
        self.resolvePermission(invoke, granted: granted, state: granted ? "GRANTED" : "DENIED")
      }
    default:
      invoke.reject("Unknown permission alias '\(alias)'", code: "UNKNOWN_PERMISSION")
    }
  }

  /// `requestPermission` — ask the user to grant a runtime permission.
  @objc public func requestPermission(_ invoke: Invoke) throws {
    guard let alias = try invoke.getArgs()["permission"] as? String else {
      invoke.reject("Missing 'permission'", code: "INVALID_ARGUMENTS")
      return
    }
    switch alias {
    case "haptics", "file_pick", "adb":
      resolvePermission(invoke, granted: true, state: "GRANTED")
    case "camera":
      if AVCaptureDevice.authorizationStatus(for: .video) == .authorized {
        resolvePermission(invoke, granted: true, state: "GRANTED")
        return
      }
      AVCaptureDevice.requestAccess(for: .video) { granted in
        self.resolvePermission(invoke, granted: granted, state: granted ? "GRANTED" : "DENIED")
      }
    case "location":
      let st = CLLocationManager.authorizationStatus()
      if st == .authorizedWhenInUse || st == .authorizedAlways {
        resolvePermission(invoke, granted: true, state: "GRANTED")
        return
      }
      let lm = CLLocationManager()
      lm.delegate = self
      authRequest = (lm, invoke)
      lm.requestWhenInUseAuthorization()
    case "notifications":
      UNUserNotificationCenter.current().requestAuthorization(options: [.alert, .sound, .badge]) {
        granted, _ in
        self.resolvePermission(invoke, granted: granted, state: granted ? "GRANTED" : "DENIED")
      }
    default:
      invoke.reject("Unknown permission alias '\(alias)'", code: "UNKNOWN_PERMISSION")
    }
  }

  private func permissionStateString(_ granted: Bool) -> String {
    granted ? "GRANTED" : "DENIED"
  }

  // ── Camera (§4.4) ────────────────────────────────────────────────────

  /// `captureCamera` — open the camera UI to take a photo.
  @objc public func captureCamera(_ invoke: Invoke) throws {
    if AVCaptureDevice.authorizationStatus(for: .video) != .authorized {
      invoke.reject("Camera permission not granted", code: "PERMISSION_DENIED")
      return
    }
    guard UIImagePickerController.isSourceTypeAvailable(.camera) else {
      invoke.reject("No camera available", code: "NO_CAMERA")
      return
    }
    guard pendingCameraInvoke == nil else {
      invoke.reject("Another camera capture is in progress", code: "BUSY")
      return
    }
    pendingCameraInvoke = invoke
    DispatchQueue.main.async {
      guard let root = self.manager.viewController else {
        self.pendingCameraInvoke = nil
        invoke.reject("No view controller to present the camera", code: "NO_VIEW_CONTROLLER")
        return
      }
      let picker = UIImagePickerController()
      picker.sourceType = .camera
      picker.delegate = self
      self.cameraPicker = picker
      root.present(picker, animated: true)
    }
  }

  // ── Location (§4.4) ──────────────────────────────────────────────────

  /// `getLocation` — return the best location fix, waiting up to 10 s.
  @objc public func getLocation(_ invoke: Invoke) throws {
    let st = CLLocationManager.authorizationStatus()
    if st == .denied || st == .restricted {
      invoke.reject("Location permission not granted", code: "PERMISSION_DENIED")
      return
    }
    let lm = CLLocationManager()
    lm.delegate = self
    lm.desiredAccuracy = kCLLocationAccuracyBest
    fixRequest = (lm, invoke)
    if st == .notDetermined {
      lm.requestWhenInUseAuthorization()
    } else {
      lm.startUpdatingLocation()
    }
    DispatchQueue.main.async { [weak self] in
      let timer = Timer(timeInterval: 10, repeats: false) { _ in
        self?.resolveLocationOrTimeout()
      }
      RunLoop.main.add(timer, forMode: .common)
      self?.fixTimer = timer
    }
  }

  private func resolveLocationOrTimeout() {
    guard let (lm, invoke) = fixRequest else { return }
    lm.stopUpdatingLocation()
    fixRequest = nil
    fixTimer?.invalidate()
    fixTimer = nil
    if let loc = lm.location {
      resolveLocation(invoke, loc)
    } else {
      invoke.reject("Timed out waiting for a location fix", code: "LOCATION_TIMEOUT")
    }
  }

  // ── Notification / haptic (§4.4) ─────────────────────────────────────

  /// `showNotification` — post a heads-up notification.
  @objc public func showNotification(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(NotifyArgs.self)
    let content = UNMutableNotificationContent()
    content.title = args.title ?? "Syscity"
    content.body = args.body ?? ""
    content.sound = .default
    content.categoryIdentifier = "agent_notify"
    let request = UNNotificationRequest(
      identifier: UUID().uuidString, content: content, trigger: nil)
    UNUserNotificationCenter.current().add(request) { error in
      if let error = error {
        invoke.reject(error.localizedDescription, code: "NOTIFY_FAILED")
      } else {
        var out = JsonObject()
        out["delivered"] = true
        invoke.resolve(JsonValue.dictionary(out))
      }
    }
  }

  /// `vibrate` — trigger a short vibration.
  @objc public func vibrate(_ invoke: Invoke) throws {
    let args = try invoke.parseArgs(VibrateArgs.self)
    let durationMs = args.duration_ms ?? 200
    // Haptic engine for iPhone; system sound as a fallback (e.g. simulator
    // has neither — harmless).
    UIImpactFeedbackGenerator(style: .medium).impactOccurred()
    AudioServicesPlaySystemSound(kSystemSoundID_Vibrate)
    var out = JsonObject()
    out["duration_ms"] = durationMs
    invoke.resolve(JsonValue.dictionary(out))
  }

  // ── File picker (§4.4, SAF analog) ───────────────────────────────────

  /// `pickFile` — open the system document picker; copy the selection into
  /// `user-files/` so the standard `file_read`/`file_write` tools work on it.
  @objc public func pickFile(_ invoke: Invoke) throws {
    guard pendingPickerInvoke == nil else {
      invoke.reject("A file pick is already open", code: "BUSY")
      return
    }
    pendingPickerInvoke = invoke
    DispatchQueue.main.async {
      guard let root = self.manager.viewController else {
        self.pendingPickerInvoke = nil
        invoke.reject("No view controller to present the picker", code: "NO_VIEW_CONTROLLER")
        return
      }
      // `.import` hands us a local copy URL in the sandbox (no security-scoped
      // access dance); we re-copy it into user-files/ for the agent tools.
      let picker = UIDocumentPickerViewController(documentTypes: ["public.item"], in: .import)
      picker.delegate = self
      picker.allowsMultipleSelection = false
      self.documentPicker = picker
      root.present(picker, animated: true)
    }
  }

  // ── Shortcuts / AppIntents bus (§4.6) ───────────────────────────────

  /// `runShortcut` — hand off to the Shortcuts app (best-effort).
  ///
  /// Opens the public `shortcuts://run-shortcut` URL scheme; the shortcut runs
  /// visibly in the Shortcuts app (foreground hand-off, not headless). A
  /// shortcut that ends with Syscity's `SyscityOutputIntent` returns its
  /// output into the sandbox for `shortcutResults` to consume.
  @objc public func runShortcut(_ invoke: Invoke) throws {
    struct RunArgs: Decodable {
      var name: String?
      var input: String?
    }
    let args = try invoke.parseArgs(RunArgs.self)
    var comps = URLComponents()
    comps.scheme = "shortcuts"
    comps.host = "run-shortcut"
    var query: [URLQueryItem] = []
    if let name = args.name, !name.isEmpty {
      query.append(URLQueryItem(name: "name", value: name))
    }
    if let input = args.input {
      query.append(URLQueryItem(name: "input", value: input))
    }
    comps.queryItems = query
    guard let url = comps.url else {
      invoke.reject("Invalid shortcut URL", code: "INVALID_ARGUMENTS")
      return
    }
    DispatchQueue.main.async {
      UIApplication.shared.open(url) { ok in
        var out = JsonObject()
        out["launched"] = ok
        out["url"] = url.absoluteString
        invoke.resolve(JsonValue.dictionary(out))
      }
    }
  }

  /// `openUrl` — hand a URL to the OS (`UIApplication.open`); the user
  /// confirms in the target app. Rust-side validation restricts schemes to
  /// tel/mailto/sms/https/http; this side re-checks and reports whether the
  /// system accepted the handoff.
  @objc public func openUrl(_ invoke: Invoke) throws {
    struct OpenArgs: Decodable {
      var url: String?
    }
    let args = try invoke.parseArgs(OpenArgs.self)
    guard let url = args.url, !url.isEmpty,
      url.rangeOfCharacter(from: .controlCharacters) == nil
    else {
      invoke.reject("Malformed URL", code: "INVALID_ARGUMENTS")
      return
    }
    let scheme = url.split(separator: ":", maxSplits: 1).first.map(String.init)?.lowercased() ?? ""
    guard ["tel", "mailto", "sms", "https", "http"].contains(scheme), let target = URL(string: url) else {
      invoke.reject("Unsupported URL scheme", code: "INVALID_ARGUMENTS")
      return
    }
    DispatchQueue.main.async {
      UIApplication.shared.open(target) { ok in
        var out = JsonObject()
        out["opened"] = ok
        out["url"] = url
        invoke.resolve(JsonValue.dictionary(out))
      }
    }
  }

  /// `shortcutResults` — list + delete-read outputs from `SyscityOutputIntent`.
  ///
  /// Each entry is `{output, at_ms, file}`. Reading a result consumes it (the
  /// file is removed) so the agent never sees the same output twice.
  @objc public func shortcutResults(_ invoke: Invoke) throws {
    consumeShortcutDir(invoke, dir: "shortcuts")
  }

  /// `shortcutInbox` — list + delete-read prompts from `AskSyscityIntent`.
  @objc public func shortcutInbox(_ invoke: Invoke) throws {
    consumeShortcutDir(invoke, dir: "shortcuts/inbox")
  }

  private func consumeShortcutDir(_ invoke: Invoke, dir: String) {
    let base = (syscityHome as NSString).appendingPathComponent(dir)
    let fm = FileManager.default
    let files = (try? fm.contentsOfDirectory(atPath: base)) ?? []
    var items = [JsonObject]()
    for f in files where f.hasSuffix(".json") {
      let path = (base as NSString).appendingPathComponent(f)
      var entry = JsonObject()
      if let data = try? Data(contentsOf: URL(fileURLWithPath: path)),
        let obj = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
      {
        entry["output"] = obj["output"] as? String
        entry["prompt"] = obj["prompt"] as? String
        entry["at_ms"] = obj["at_ms"] as? NSNumber
      }
      entry["file"] = f
      items.append(entry)
      try? fm.removeItem(atPath: path)
    }
    var out = JsonObject()
    out["items"] = items
    invoke.resolve(JsonValue.dictionary(out))
  }

  /// Dead-strip anchor for the AppIntent types (§4.6).
  ///
  /// Nothing in the app links directly against `SyscityOutputIntent` /
  /// `AskSyscityIntent` — the system discovers them at runtime — so the
  /// linker would drop `AppIntents.swift.o` from the static archive and the
  /// intents would never appear in Shortcuts. This `@objc` method lives in
  /// `DevicePlugin.swift.o` (kept alive via the Rust-referenced
  /// `init_plugin_syscity_device`), and referencing both metadata accessors
  /// forces the linker to extract the AppIntents object.
  @objc public func appIntentAnchor(_ invoke: Invoke) throws {
    var out = JsonObject()
    if #available(iOS 16.0, *) {
      let anchors: [Any.Type] = [SyscityOutputIntent.self, AskSyscityIntent.self]
      out["intent_types"] = anchors.count
    } else {
      out["intent_types"] = 0
    }
    invoke.resolve(JsonValue.dictionary(out))
  }

  // ── Cron background wake (§4.3 parity, best-effort) ─────────────────

  /// `syncCronSchedule` — re-arm local notifications from a schedule snapshot.
  ///
  /// iOS has no WorkManager analogue; the honest best-effort is to schedule a
  /// local notification per due job to nudge the user. When the app opens, the
  /// gateway re-arms from jobs.json and runs due jobs (persistence handles
  /// this), exactly like the Android wake.
  @objc public func syncCronSchedule(_ invoke: Invoke) throws {
    let args = try invoke.getArgs()
    let jobs = args["jobs"] as? [[String: Any]] ?? []
    let center = UNUserNotificationCenter.current()
    var scheduled = 0
    for job in jobs {
      guard let id = job["id"] as? String, !id.isEmpty,
        let atMs = job["at_ms"] as? NSNumber
      else {
        continue
      }
      let fire = Date(timeIntervalSince1970: atMs.doubleValue / 1000.0)
      let content = UNMutableNotificationContent()
      content.title = "Syscity task due"
      content.body = "Scheduled task '\(id)' is due. Open Syscity to run it."
      content.sound = .default
      content.categoryIdentifier = "agent_notify"
      let interval = max(0.1, fire.timeIntervalSinceNow)
      let trigger = UNTimeIntervalNotificationTrigger(timeInterval: interval, repeats: false)
      center.add(
        UNNotificationRequest(identifier: "cron-\(id)", content: content, trigger: trigger))
      scheduled += 1
    }
    var out = JsonObject()
    out["scheduled"] = scheduled
    invoke.resolve(JsonValue.dictionary(out))
  }
}

// ── Delegate conformance ────────────────────────────────────────────────

extension DevicePlugin: UIImagePickerControllerDelegate, UINavigationControllerDelegate {
  func imagePickerController(
    _ picker: UIImagePickerController,
    didFinishPickingMediaWithInfo info: [UIImagePickerController.InfoKey: Any]
  ) {
    picker.dismiss(animated: true)
    let invoke = pendingCameraInvoke
    pendingCameraInvoke = nil
    cameraPicker = nil
    guard let image = info[.originalImage] as? UIImage,
      let data = image.jpegData(compressionQuality: 0.85)
    else {
      invoke?.reject("Camera produced no image", code: "CAPTURE_FAILED")
      return
    }
    let dir = (syscityHome as NSString).appendingPathComponent("camera")
    try? FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
    let name = "IMG_\(DevicePlugin.timestamp()).jpg"
    let path = (dir as NSString).appendingPathComponent(name)
    do {
      try data.write(to: URL(fileURLWithPath: path))
      var out = JsonObject()
      out["path"] = "camera/\(name)"
      out["width"] = Int(image.size.width)
      out["height"] = Int(image.size.height)
      invoke?.resolve(JsonValue.dictionary(out))
    } catch {
      invoke?.reject("Failed to save photo: \(error.localizedDescription)", code: "WRITE_FAILED")
    }
  }

  func imagePickerControllerDidCancel(_ picker: UIImagePickerController) {
    picker.dismiss(animated: true)
    let invoke = pendingCameraInvoke
    pendingCameraInvoke = nil
    cameraPicker = nil
    invoke?.reject("Camera capture cancelled", code: "CANCELLED")
  }
}

extension DevicePlugin: UIDocumentPickerDelegate {
  func documentPicker(
    _ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]
  ) {
    controller.dismiss(animated: true)
    guard let url = urls.first else {
      let invoke = pendingPickerInvoke
      pendingPickerInvoke = nil
      invoke?.reject("File pick cancelled", code: "CANCELLED")
      return
    }
    let safeName = DevicePlugin.sanitizeName(url.lastPathComponent)
    DispatchQueue.global(qos: .userInitiated).async {
      do {
        let dir = (self.syscityHome as NSString).appendingPathComponent("user-files")
        try FileManager.default.createDirectory(atPath: dir, withIntermediateDirectories: true)
        let target = DevicePlugin.uniqueTarget(dir: dir, name: safeName)
        if FileManager.default.fileExists(atPath: url.path) {
          try FileManager.default.copyItem(at: url, to: URL(fileURLWithPath: target))
        } else {
          try FileManager.default.createDirectory(
            atPath: (target as NSString).deletingLastPathComponent, withIntermediateDirectories: true)
          let data = try Data(contentsOf: url)
          try data.write(to: URL(fileURLWithPath: target))
        }
        let attrs = try? FileManager.default.attributesOfItem(atPath: target)
        let size = (attrs?[.size] as? NSNumber)?.intValue ?? 0
        var out = JsonObject()
        out["path"] = "user-files/\((target as NSString).lastPathComponent)"
        out["name"] = (target as NSString).lastPathComponent
        out["size_bytes"] = size
        let invoke = self.pendingPickerInvoke
        self.pendingPickerInvoke = nil
        invoke?.resolve(JsonValue.dictionary(out))
      } catch {
        let invoke = self.pendingPickerInvoke
        self.pendingPickerInvoke = nil
        invoke?.reject(error.localizedDescription, code: "COPY_FAILED")
      }
    }
  }

  func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
    let invoke = pendingPickerInvoke
    pendingPickerInvoke = nil
    invoke?.reject("File pick cancelled", code: "CANCELLED")
  }
}

extension DevicePlugin: CLLocationManagerDelegate {
  func locationManager(_ manager: CLLocationManager, didUpdateLocations locations: [CLLocation]) {
    guard let (lm, invoke) = fixRequest, lm === manager, let loc = locations.first else {
      return
    }
    lm.stopUpdatingLocation()
    fixRequest = nil
    fixTimer?.invalidate()
    fixTimer = nil
    resolveLocation(invoke, loc)
  }

  func locationManager(_ manager: CLLocationManager, didFailWithError error: Error) {
    guard let (lm, invoke) = fixRequest, lm === manager else { return }
    lm.stopUpdatingLocation()
    fixRequest = nil
    fixTimer?.invalidate()
    fixTimer = nil
    invoke.reject(error.localizedDescription, code: "LOCATION_ERROR")
  }

  // iOS 14+ authorization callback.
  func locationManagerDidChangeAuthorization(_ manager: CLLocationManager) {
    let st = CLLocationManager.authorizationStatus()
    let granted = st == .authorizedWhenInUse || st == .authorizedAlways
    if let (lm, invoke) = authRequest, lm === manager {
      authRequest = nil
      resolvePermission(invoke, granted: granted, state: granted ? "GRANTED" : "DENIED")
      return
    }
    if let (lm, invoke) = fixRequest, lm === manager {
      if granted {
        lm.startUpdatingLocation()
      } else {
        fixRequest = nil
        fixTimer?.invalidate()
        fixTimer = nil
        invoke.reject("Location permission not granted", code: "PERMISSION_DENIED")
      }
    }
  }

  // Pre-iOS-14 fallback (deployment target is 14; kept for safety).
  func locationManager(_ manager: CLLocationManager, didChangeAuthorization status: CLAuthorizationStatus) {
    locationManagerDidChangeAuthorization(manager)
  }
}

// ── Keyboard accessory bar ("Done" button) ──────────────────────────────

private var didHideInputAccessory = false

/// The WKWebView input accessory bar — the "Done" button above the keyboard —
/// is native WebKit UI that no web-side CSS/JS can remove. Replace
/// `WKContentView.inputAccessoryView` with an IMP returning nil so the bar
/// never appears. Runs once; if WKContentView isn't registered yet (it is by
/// `load(webview:)` time, since the webview already exists), the flag is only
/// set on success so a later call can retry.
private func hideWebViewInputAccessory() {
  guard !didHideInputAccessory else { return }
  guard let cls = NSClassFromString("WKContentView") as? AnyClass else { return }
  let sel = NSSelectorFromString("inputAccessoryView")
  guard let method = class_getInstanceMethod(cls, sel) else { return }
  let encoding = method_getTypeEncoding(method)
  let block: @convention(block) (AnyObject) -> UIView? = { _ in nil }
  class_replaceMethod(cls, sel, imp_implementationWithBlock(block), encoding)
  didHideInputAccessory = true
}

// ── Entry point ─────────────────────────────────────────────────────────

@_cdecl("init_plugin_syscity_device")
func initPlugin() -> Plugin {
  return DevicePlugin()
}
