//! Syscity Desktop App — Tauri backend
//!
//! Embeds the Syscity Gateway and serves it to the Tauri WebView.

use std::sync::Arc;

use tauri::{Emitter, Manager};
use tokio::sync::Mutex;

mod connection;
use connection::{ConnectionConfig, ConnectionMode};

/// A writer that duplicates output to both stdout and a log file.
///
/// This is used so that release builds (where stdout is disconnected)
/// still have a persistent log trail in `~/.syscity/logs/desktop.log`.
struct DualWriter {
    stdout: std::io::Stdout,
    file: Arc<std::sync::Mutex<std::fs::File>>,
}

impl Clone for DualWriter {
    fn clone(&self) -> Self {
        Self {
            stdout: std::io::stdout(),
            file: Arc::clone(&self.file),
        }
    }
}

impl std::io::Write for DualWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.stdout.write(buf)?;
        let _ = self.file.lock().unwrap().write_all(buf);
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stdout.flush()?;
        let _ = self.file.lock().unwrap().flush();
        Ok(())
    }
}

/// Shared application state between Tauri commands.
pub struct AppState {
    /// Whether the embedded Gateway has finished startup.
    pub gateway_ready: bool,
    /// The actual port the Gateway bound to (auto-detected).
    pub gateway_port: u16,
    /// Full gateway base URL (http://host:port) the frontend should use.
    /// Loopback for local mode; the remote host in remote mode.
    pub gateway_base: String,
    /// Per-install gateway auth token (mobile only; `None` on desktop, where
    /// `auth_mode = "none"` remains the default).
    pub gateway_token: Option<String>,
}

/// Native device bridge (mobile only; mobile-migration §4.1/§4.2/§4.5).
///
/// Registered as the `"device"` Tauri plugin; the plugin's setup callback
/// calls `register_android_plugin` and stores the resulting [`PluginHandle`]
/// in managed state so the gateway task can wrap it as a
/// [`syscity::device::DeviceBridge`]. Desktop builds never compile this
/// module (`#[cfg(mobile)]` is set by tauri-build only for android/ios).
#[cfg(mobile)]
mod mobile_device {
    use std::sync::Arc;

    use tauri::plugin::PluginHandle;
    use tauri::Manager;

    /// Tauri-managed state holding the registered device plugin handle.
    #[derive(Clone)]
    pub(crate) struct TauriDeviceBridge {
        plugin: PluginHandle<tauri::Wry>,
    }

    #[async_trait::async_trait]
    impl syscity::device::DeviceBridge for TauriDeviceBridge {
        async fn call(
            &self,
            command: &str,
            payload: serde_json::Value,
        ) -> syscity::Result<serde_json::Value> {
            self.plugin
                .run_mobile_plugin_async::<serde_json::Value>(command, payload)
                .await
                .map_err(|e| {
                    syscity::error::SyscityError::Internal(format!(
                        "Device command '{}' failed: {}",
                        command, e
                    ))
                })
        }
    }

    /// Build the `"device"` plugin. Its setup registers the Kotlin
    /// `DevicePlugin` (android) and stores the handle in managed state.
    /// Tauri mobile apps always use the `Wry` runtime.
    pub(crate) fn device_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
        tauri::plugin::Builder::new("device")
            .setup(|app, api| {
                #[cfg(target_os = "android")]
                {
                    let handle =
                        api.register_android_plugin("net.syscity.desktop", "DevicePlugin")?;
                    app.manage(TauriDeviceBridge { plugin: handle });
                }
                #[cfg(target_os = "ios")]
                {
                    tauri::ios_plugin_binding!(init_plugin_syscity_device);
                    let handle = api.register_ios_plugin(init_plugin_syscity_device)?;
                    app.manage(TauriDeviceBridge { plugin: handle });
                }
                Ok(())
            })
            .build()
    }

    /// The bridge if the plugin was registered, else `None` (e.g. iOS).
    pub(crate) fn bridge_from_app(
        app: &tauri::AppHandle,
    ) -> Option<Arc<dyn syscity::device::DeviceBridge>> {
        app.try_state::<TauriDeviceBridge>()
            .map(|b| Arc::new(b.inner().clone()) as Arc<dyn syscity::device::DeviceBridge>)
    }
}

/// Native speech-recognition bridge (mobile only).
///
/// Unlike the device bridge, nothing on the gateway side calls this plugin —
/// the WebView invokes `plugin:speech|*` commands directly to drive the
/// composer voice mode — so no managed state is kept. Android registers the
/// Kotlin `SpeechPlugin` (SpeechRecognizer); iOS registers the Swift
/// `SpeechPlugin` (SFSpeechRecognizer). Both share the same command/event
/// contract so the web layer drives them identically.
#[cfg(mobile)]
mod mobile_speech {
    /// Build the `"speech"` plugin.
    pub(crate) fn speech_plugin() -> tauri::plugin::TauriPlugin<tauri::Wry> {
        tauri::plugin::Builder::new("speech")
            .setup(|_app, api| {
                #[cfg(target_os = "android")]
                api.register_android_plugin("net.syscity.desktop", "SpeechPlugin")?;
                #[cfg(target_os = "ios")]
                {
                    tauri::ios_plugin_binding!(init_plugin_syscity_speech);
                    let _ = api.register_ios_plugin(init_plugin_syscity_speech)?;
                }
                Ok(())
            })
            .build()
    }
}

/// Tauri command: returns the Gateway base URL for the frontend.
#[tauri::command]
fn get_api_url(state: tauri::State<'_, Arc<Mutex<AppState>>>) -> String {
    let state = state.blocking_lock();
    state.gateway_base.clone()
}

/// Tauri command: current connection settings (local vs remote gateway).
#[tauri::command]
fn get_connection() -> ConnectionConfig {
    connection::load_connection()
}

/// Tauri command: compile-time OS ("macos"|"windows"|"linux"|"ios"|"android").
/// The frontend shell uses it to pick the titlebar variant (macOS overlay
/// inset + drag region vs plain header on other platforms / in browsers).
#[tauri::command]
fn get_platform() -> String {
    std::env::consts::OS.to_string()
}

/// Tauri command: persist connection settings.
#[tauri::command]
fn save_connection(config: ConnectionConfig) -> Result<(), String> {
    connection::save_connection(&config)
}

/// Tauri command: probe a remote gateway — reachability via `/live` and
/// authentication via a WebSocket upgrade with the token.
#[tauri::command]
async fn test_remote_gateway(
    host: String,
    port: u16,
    token: Option<String>,
) -> Result<String, String> {
    // 1. Liveness (no auth needed — /live is in the public tier).
    if !gateway_healthy(&host, port).await {
        return Err(format!(
            "Cannot reach gateway at {}:{} — is it running and bound to a reachable address?",
            host, port
        ));
    }
    // 2. WS upgrade with the token: 101 = authenticated, 401 = bad token.
    let probe = ws_upgrade_probe(&host, port, token.as_deref()).await;
    match probe.as_str() {
        "ok" => Ok(format!("Gateway reachable and authenticated at {}:{}", host, port)),
        "unauthorized" => Err("Gateway reachable, but the token was rejected (401).".to_string()),
        _ => Err(format!("Gateway reachable, but the WebSocket handshake failed: {}", probe)),
    }
}

/// Tauri command: the gateway auth token the WebView presents at the WS
/// handshake / as an HTTP Bearer credential.
///
/// Mobile always requires a per-install token (loopback is shared between
/// apps). Desktop requires one only in remote mode, where it is the remote
/// gateway's shared token from the connection config; local mode has none.
#[tauri::command]
fn get_gateway_token(state: tauri::State<'_, Arc<Mutex<AppState>>>) -> Option<String> {
    state.blocking_lock().gateway_token.clone()
}

/// Load the per-install gateway token, generating one on first launch.
///
/// Stored at `<SYSCITY_HOME>/data/gateway_token` — the app sandbox already
/// protects the file; the token exists because loopback is shared with every
/// other installed app.
#[cfg(mobile)]
fn load_or_create_gateway_token() -> String {
    use rand::RngCore;

    let path = syscity::dirs::data_dir().join("gateway_token");
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if trimmed.len() >= 32 {
            return trimmed.to_string();
        }
    }

    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let token: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Err(e) = std::fs::write(&path, &token) {
        eprintln!("Failed to persist gateway token to {:?}: {}", path, e);
    }
    token
}

/// Tauri command: reveals an artifact file in the system file manager.
///
/// Platform behaviour:
/// - macOS: `open -R` reveals the file in Finder.
/// - Windows: `explorer /select,` reveals the file in Explorer.
/// - Linux: opens the parent folder in the default file manager.
#[tauri::command]
fn reveal_in_folder(filename: String) -> Result<(), String> {
    let path = syscity::dirs::syscity_dir()
        .join("artifacts")
        .join(&filename);

    // Path traversal check (defence-in-depth).
    if filename.contains("..") || filename.contains('/') || filename.contains('\\') {
        return Err("Invalid filename".into());
    }

    if !path.exists() {
        return Err(format!("File not found: {}", filename));
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("Failed to open Finder: {}", e))?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("explorer")
            .arg("/select,")
            .arg(&path)
            .spawn()
            .map_err(|e| format!("Failed to open Explorer: {}", e))?;
    }
    #[cfg(target_os = "linux")]
    {
        let folder = path.parent().unwrap_or(&path);
        std::process::Command::new("xdg-open")
            .arg(folder)
            .spawn()
            .map_err(|e| format!("Failed to open file manager: {}", e))?;
    }

    Ok(())
}

/// Tauri command: check for a newer release, download + install it, and
/// restart the app. Emits `update-status` events so the frontend can show
/// progress. Desktop only — mobile apps update through their app stores.
#[cfg(not(mobile))]
#[tauri::command]
async fn check_for_updates(app: tauri::AppHandle) -> Result<String, String> {
    run_update_check(app).await
}

/// Shared update logic for the `check_for_updates` command and the tray item.
#[cfg(not(mobile))]
async fn run_update_check(app: tauri::AppHandle) -> Result<String, String> {
    use tauri_plugin_updater::UpdaterExt;

    let updater = app
        .updater()
        .map_err(|e| format!("Updater not available: {e}"))?;

    match updater.check().await {
        Ok(Some(update)) => {
            app.emit("update-status", "downloading").ok();
            update
                .download_and_install(|_current, _total| {}, || {})
                .await
                .map_err(|e| format!("Failed to install update: {e}"))?;
            app.emit("update-status", "restarting").ok();
            // `restart` never returns — the process is replaced immediately.
            app.restart()
        }
        Ok(None) => Ok("up-to-date".into()),
        Err(e) => Err(format!("Update check failed: {e}")),
    }
}

/// Entry point used by `src/main.rs`.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Initialize syscity global setup (panic handler, etc.)
    if let Err(e) = syscity::init() {
        eprintln!("Failed to initialize syscity: {}", e);
        std::process::exit(1);
    }

    // Pin the process-wide path root before anything resolves a path: the
    // desktop app is a single-root process, and `dirs::` free functions (used
    // just below for the log file) panic if no root was installed.
    if syscity::dirs::set_default_paths(Arc::new(syscity::dirs::SyscityPaths::from_env())).is_err()
    {
        // Already installed — harmless, both resolve to the same root.
        eprintln!("path root already installed; keeping the existing one");
    }

    // Set up a simple tracing subscriber so Gateway logs are visible.
    // In release builds stdout is disconnected (windows_subsystem), so we
    // also duplicate everything to a log file.
    let log_path = syscity::dirs::logs_dir().join("desktop.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let log_file = Arc::new(std::sync::Mutex::new(
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .expect("Failed to open desktop log file"),
    ));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(move || DualWriter {
            stdout: std::io::stdout(),
            file: Arc::clone(&log_file),
        })
        .try_init();

    // Remote mode exposes the remote gateway's shared token to the frontend
    // (WS query / HTTP Bearer). Local mode uses the per-install token on
    // mobile (loopback is shared between apps) and none on desktop.
    let conn = connection::load_connection();
    let gateway_token: Option<String> = match conn.mode {
        ConnectionMode::Remote => conn.token,
        ConnectionMode::Local => {
            #[cfg(mobile)]
            {
                Some(load_or_create_gateway_token())
            }
            #[cfg(not(mobile))]
            {
                None
            }
        }
    };

    let app_state = Arc::new(Mutex::new(AppState {
        gateway_ready: false,
        gateway_port: 18080,
        gateway_base: "http://127.0.0.1:18080".to_string(),
        gateway_token,
    }));

    let app_state_for_setup = app_state.clone();

    let builder = tauri::Builder::default()
        .manage(app_state)
        .plugin(tauri_plugin_shell::init());

    // Self-update via the Tauri updater (desktop only; mobile apps update
    // through their app stores). Registration is inert until a minisign
    // pubkey is set in `plugins.updater.pubkey` — see desktop/README or the
    // Part 1 release prerequisites.
    #[cfg(not(mobile))]
    let builder = builder.plugin(tauri_plugin_updater::Builder::new().build());

    // Native device bridge plugin (mobile only): registers the Kotlin
    // DevicePlugin (android) or the Swift DevicePlugin (ios) so the gateway
    // can reach camera / location / notifications / SAF / shortcuts. Must be
    // added on the builder chain — `App::plugin` is not available for Wry.
    #[cfg(mobile)]
    let builder = builder.plugin(mobile_device::device_plugin());

    // Native speech recognition (mobile only): the WebView invokes
    // `plugin:speech|*` directly for composer voice input.
    #[cfg(mobile)]
    let builder = builder.plugin(mobile_speech::speech_plugin());

    #[cfg(not(mobile))]
    let builder = builder.invoke_handler(tauri::generate_handler![
        get_api_url,
        get_gateway_token,
        reveal_in_folder,
        check_for_updates,
        get_connection,
        get_platform,
        save_connection,
        test_remote_gateway
    ]);
    #[cfg(mobile)]
    let builder = builder.invoke_handler(tauri::generate_handler![
        get_api_url,
        reveal_in_folder,
        get_gateway_token,
        get_connection,
        get_platform,
        save_connection,
        test_remote_gateway
    ]);

    builder
        .setup(move |app| {
            let handle = app.handle().clone();
            let state = app_state_for_setup.clone();

            // Spawn the Syscity Gateway in a background task. The connection
            // mode (from ~/.syscity/client.toml) decides: remote mode connects
            // to a gateway on another host; local mode runs the embedded
            // gateway (mobile, or desktop with nothing running) or reuses an
            // already-running local gateway (desktop, e.g. the CLI daemon).
            tauri::async_runtime::spawn(async move {
                #[cfg(mobile)]
                let device_bridge: Option<
                    std::sync::Arc<dyn syscity::device::DeviceBridge>,
                > = mobile_device::bridge_from_app(&handle);
                #[cfg(not(mobile))]
                let device_bridge: Option<
                    std::sync::Arc<dyn syscity::device::DeviceBridge>,
                > = None;

                let conn = connection::load_connection();
                match conn.mode {
                    ConnectionMode::Remote => {
                        // Remote mode: connect to the configured gateway.
                        let base = connection::remote_base(&conn);
                        eprintln!("Connecting to remote Syscity Gateway at {}", base);
                        if !gateway_healthy(&conn.host, conn.port).await {
                            eprintln!("Remote gateway {} is not reachable (is it running?)", base);
                        }
                        let mut s = state.lock().await;
                        s.gateway_port = conn.port;
                        s.gateway_base = base.clone();
                        // The remote gateway requires its shared token at the WS
                        // handshake and as an HTTP Bearer — expose it to the
                        // frontend (get_gateway_token).
                        s.gateway_token = conn.token.clone();
                        drop(s);
                        announce_gateway_ready(&handle, &conn.host, conn.port).await;
                        watch_remote_gateway(handle, conn).await;
                    }
                    ConnectionMode::Local => {
                        #[cfg(mobile)]
                        {
                            // Mobile local: the embedded gateway with the
                            // per-install token (loopback is shared between apps).
                            let port = match find_available_port("127.0.0.1", 18080, 100).await {
                                Some(p) => p,
                                None => {
                                    eprintln!("No available port found in range 18080-18179");
                                    return;
                                }
                            };
                            let mut s = state.lock().await;
                            s.gateway_port = port;
                            s.gateway_base = format!("http://127.0.0.1:{}", port);
                            drop(s);
                            let h2 = handle.clone();
                            tauri::async_runtime::spawn(async move {
                                monitor_owned_gateway(h2, port).await;
                            });
                            if let Err(e) = start_gateway(handle.clone(), port, device_bridge).await
                            {
                                eprintln!("Gateway failed: {}", e);
                            }
                        }
                        #[cfg(not(mobile))]
                        {
                            // Desktop local: reuse an existing local gateway
                            // (e.g. the CLI daemon) or start the embedded one.
                            let configured = configured_gateway_port().await;
                            if gateway_healthy("127.0.0.1", configured).await {
                                eprintln!(
                                    "Reusing existing Syscity Gateway on http://127.0.0.1:{}",
                                    configured
                                );
                                let mut s = state.lock().await;
                                s.gateway_port = configured;
                                s.gateway_base = format!("http://127.0.0.1:{}", configured);
                                drop(s);
                                announce_gateway_ready(&handle, "127.0.0.1", configured).await;
                                watch_reused_gateway(handle, state, configured, device_bridge)
                                    .await;
                            } else {
                                match find_available_port("127.0.0.1", configured, 100).await {
                                    Some(port) => {
                                        let mut s = state.lock().await;
                                        s.gateway_port = port;
                                        s.gateway_base = format!("http://127.0.0.1:{}", port);
                                        drop(s);
                                        let h2 = handle.clone();
                                        tauri::async_runtime::spawn(async move {
                                            monitor_owned_gateway(h2, port).await;
                                        });
                                        if let Err(e) =
                                            start_gateway(handle.clone(), port, device_bridge).await
                                        {
                                            eprintln!("Gateway failed: {}", e);
                                        }
                                    }
                                    None => {
                                        eprintln!(
                                            "No available port found in range {}-{}",
                                            configured,
                                            configured + 99
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            });

            // Set up system tray and menu (desktop only — no tray on mobile).
            #[cfg(not(mobile))]
            if let Err(e) = setup_tray_and_menu(app) {
                eprintln!("Tray/menu setup failed: {}", e);
            }

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// Probe for an available TCP port starting at `start`.
async fn find_available_port(host: &str, start: u16, max_attempts: u16) -> Option<u16> {
    let end = start + max_attempts;
    for port in start..end {
        match tokio::net::TcpListener::bind(format!("{}:{}", host, port)).await {
            Ok(listener) => {
                // Port is available; drop the listener so Gateway can bind it.
                drop(listener);
                return Some(port);
            }
            Err(_) => continue,
        }
    }
    None
}

/// Read the configured gateway port from `~/.syscity/config.toml`
/// (defaults to 18080 when the config is missing or unparseable).
/// Desktop only — mobile always runs the embedded gateway.
#[cfg(not(mobile))]
async fn configured_gateway_port() -> u16 {
    use syscity::gateway::GatewayConfig;
    let config_path = syscity::dirs::default_config_file();
    if let Ok(content) = tokio::fs::read_to_string(&config_path).await {
        if let Ok(config) = toml::from_str::<GatewayConfig>(&content) {
            return config.port;
        }
    }
    18080
}

/// Probe `GET /live` on `host:port` to detect an already-running gateway.
///
/// `/live` returns 200 whenever the process is up, unlike `/health` which is
/// 503 until the gateway reports fully healthy — so this answers "is a gateway
/// running here?" rather than "is it healthy?".
async fn gateway_healthy(host: &str, port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(mut stream) = tokio::net::TcpStream::connect((host, port)).await else {
        return false;
    };
    let request = format!("GET /live HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\r\n", host);
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut buf = [0u8; 512];
    let Ok(n) = stream.read(&mut buf).await else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200")
}

/// Probe the gateway WebSocket upgrade with an optional token.
///
/// Returns `"ok"` on a 101 (authenticated), `"unauthorized"` on a 401 (bad /
/// missing token), or a short description of the failure otherwise. Used by
/// the remote-connection "test" flow and the startup remote probe.
async fn ws_upgrade_probe(host: &str, port: u16, token: Option<&str>) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Ok(mut stream) = tokio::net::TcpStream::connect((host, port)).await else {
        return "unreachable".to_string();
    };
    let key = "x3JJHMbDL1EzLkh9GBhXDw=="; // fixed test key; the server does not validate it
    let mut request = format!(
        "GET /ws HTTP/1.1\r\nHost: {}:{}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {}\r\n",
        host, port, key
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {}\r\n", token));
    }
    request.push_str("\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return "write failed".to_string();
    }
    let mut buf = [0u8; 512];
    let Ok(n) = stream.read(&mut buf).await else {
        return "no response".to_string();
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    if head.starts_with("HTTP/1.1 101") || head.starts_with("HTTP/1.0 101") {
        "ok".to_string()
    } else if head.starts_with("HTTP/1.1 401") || head.starts_with("HTTP/1.0 401") {
        "unauthorized".to_string()
    } else {
        head.lines().next().unwrap_or("unknown").to_string()
    }
}

/// Show the main window and notify the frontend that a gateway is ready.
async fn announce_gateway_ready(handle: &tauri::AppHandle, host: &str, port: u16) {
    if let Some(window) = handle.get_webview_window("main") {
        window.show().unwrap();
    }
    handle
        .emit("gateway-ready", format!("http://{}:{}", host, port))
        .ok();
}

/// Periodically probe a gateway we own (the embedded one) and emit
/// `gateway-down` if it stops responding, so the WebView can surface it.
async fn monitor_owned_gateway(handle: tauri::AppHandle, port: u16) {
    let probe_interval = std::time::Duration::from_secs(10);
    loop {
        tokio::time::sleep(probe_interval).await;
        if gateway_healthy("127.0.0.1", port).await {
            continue;
        }
        eprintln!("Embedded gateway on 127.0.0.1:{} stopped responding", port);
        let _ = handle.emit("gateway-down", format!("http://127.0.0.1:{}", port));
        return;
    }
}

/// Desktop only: watch a reused (external) gateway and fall back to the
/// embedded gateway when it stops responding.
#[cfg(not(mobile))]
async fn watch_reused_gateway(
    handle: tauri::AppHandle,
    state: Arc<tokio::sync::Mutex<AppState>>,
    port: u16,
    device_bridge: Option<std::sync::Arc<dyn syscity::device::DeviceBridge>>,
) {
    let probe_interval = std::time::Duration::from_secs(5);
    loop {
        tokio::time::sleep(probe_interval).await;
        if gateway_healthy("127.0.0.1", port).await {
            continue;
        }
        eprintln!(
            "External gateway on 127.0.0.1:{} stopped responding — starting the embedded gateway",
            port
        );
        break;
    }

    // Fall back to the embedded gateway on the next free port.
    let configured = configured_gateway_port().await;
    let Some(new_port) = find_available_port("127.0.0.1", configured, 100).await else {
        eprintln!("No available port found in range {}-{}", configured, configured + 99);
        return;
    };
    {
        let mut s = state.lock().await;
        s.gateway_port = new_port;
        s.gateway_base = format!("http://127.0.0.1:{}", new_port);
    }

    // Watch the embedded gateway too, once it is running.
    let h2 = handle.clone();
    tauri::async_runtime::spawn(async move {
        monitor_owned_gateway(h2, new_port).await;
    });

    if let Err(e) = start_gateway(handle, new_port, device_bridge).await {
        eprintln!("Embedded gateway failed to start: {}", e);
    }
}

/// Watch a remote gateway and keep the frontend in sync when it drops and
/// recovers. No local fallback — the user explicitly chose remote.
async fn watch_remote_gateway(handle: tauri::AppHandle, conn: ConnectionConfig) {
    let probe_interval = std::time::Duration::from_secs(10);
    let base = connection::remote_base(&conn);
    loop {
        tokio::time::sleep(probe_interval).await;
        if gateway_healthy(&conn.host, conn.port).await {
            continue;
        }
        eprintln!("Remote gateway {} stopped responding", base);
        let _ = handle.emit("gateway-down", base.clone());
        // Keep watching so the frontend recovers when the remote comes back.
        loop {
            tokio::time::sleep(probe_interval).await;
            if gateway_healthy(&conn.host, conn.port).await {
                eprintln!("Remote gateway {} is back", base);
                let _ = handle.emit("gateway-ready", base.clone());
                break;
            }
        }
    }
}

/// Start the embedded Syscity Gateway on the given port.
async fn start_gateway(
    handle: tauri::AppHandle,
    port: u16,
    device_bridge: Option<std::sync::Arc<dyn syscity::device::DeviceBridge>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use syscity::gateway::{Gateway, GatewayConfig, GatewayOptions};

    // The desktop app embeds the gateway in-process, so the binary-replacement
    // update flow (`POST /api/v1/update`) must be refused: this instance is
    // updated by replacing the whole app bundle via the Tauri updater instead.
    // The web UI reads this flag and routes the update button to `invoke`.
    std::env::set_var("SYSCITY_EMBEDDED", "1");

    // Ensure ~/.syscity directory exists.
    let syscity_dir = syscity::dirs::syscity_dir();
    let config_path = syscity::dirs::default_config_file();

    if !syscity_dir.exists() {
        tokio::fs::create_dir_all(&syscity_dir)
            .await
            .map_err(|e| format!("Failed to create syscity dir: {}", e))?;
    }

    // Load or create default Gateway config.
    let mut gateway_config = if config_path.exists() {
        match tokio::fs::read_to_string(&config_path).await {
            Ok(content) => match toml::from_str::<GatewayConfig>(&content) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("Failed to parse config.toml: {}, using defaults", e);
                    GatewayConfig::default()
                }
            },
            Err(e) => {
                eprintln!("Failed to read config.toml: {}, using defaults", e);
                GatewayConfig::default()
            }
        }
    } else {
        // Serialize the default config instead of hand-writing a template:
        // a hand-written string can silently drift out of sync with
        // GatewayConfig's schema (required fields like `security.rate_limit`
        // and the flat `model`/`model_provider` keys) and then fail to
        // re-parse on the next start, silently falling back to defaults.
        let default_config = toml::to_string_pretty(&GatewayConfig::default())
            .map_err(|e| format!("Failed to serialize default config: {}", e))?;
        tokio::fs::write(&config_path, default_config)
            .await
            .map_err(|e| format!("Failed to write default config: {}", e))?;
        GatewayConfig::default()
    };

    // Force localhost binding and use the auto-detected port.
    gateway_config.host = "127.0.0.1".to_string();
    gateway_config.port = port;

    // Mobile builds must never run unauthenticated: loopback is shared with
    // every installed app. Force shared-token auth using the per-install
    // token generated at first launch. Desktop keeps `auth_mode = "none"`.
    #[cfg(mobile)]
    {
        let token = handle
            .state::<Arc<Mutex<AppState>>>()
            .lock()
            .await
            .gateway_token
            .clone();
        if let Some(token) = token {
            gateway_config.security.enabled = true;
            gateway_config.security.auth_required = true;
            gateway_config.security.auth_mode = syscity::gateway::protocol::AuthMode::Token;
            gateway_config.security.shared_token = Some(token);
        }
    }

    // Configure LLM provider from environment (same logic as daemon.rs).
    if let (Ok(base_url), Ok(api_key)) =
        (std::env::var("SYSCITY_BASE_URL"), std::env::var("SYSCITY_API_KEY"))
    {
        let provider_config = syscity::model_router::ProviderConfig {
            provider_type: syscity::model_router::ProviderType::OpenAi,
            models: Vec::new(),
            default_model: String::new(),
            api_key: api_key.into(),
            api_keys: Vec::new(),
            auth_profile: None,
            oauth: None,
            base_url: Some(base_url),
            timeout: std::time::Duration::from_secs(60),
            max_retries: 3,
            retry_delay_ms: 1000,
        };
        gateway_config
            .providers
            .insert("openai".to_string(), provider_config);
    } else if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        let provider_config = syscity::model_router::ProviderConfig {
            provider_type: syscity::model_router::ProviderType::Anthropic,
            models: Vec::new(),
            default_model: String::new(),
            api_key: api_key.into(),
            api_keys: Vec::new(),
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: std::time::Duration::from_secs(60),
            max_retries: 3,
            retry_delay_ms: 1000,
        };
        gateway_config
            .providers
            .insert("anthropic".to_string(), provider_config);
    } else if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
        let provider_config = syscity::model_router::ProviderConfig {
            provider_type: syscity::model_router::ProviderType::OpenAi,
            models: Vec::new(),
            default_model: String::new(),
            api_key: api_key.into(),
            api_keys: Vec::new(),
            auth_profile: None,
            oauth: None,
            base_url: None,
            timeout: std::time::Duration::from_secs(60),
            max_retries: 3,
            retry_delay_ms: 1000,
        };
        gateway_config
            .providers
            .insert("openai".to_string(), provider_config);
    }

    eprintln!(
        "Starting Syscity Gateway on http://{}:{}",
        gateway_config.host, gateway_config.port
    );

    let gateway = Gateway::with_options(
        gateway_config.clone(),
        Some(config_path),
        GatewayOptions {
            device_bridge,
            ..Default::default()
        },
    )
    .await
    .map_err(|e| format!("Failed to create gateway: {}", e))?;

    // Show the main window and notify the frontend now that the backend is ready.
    announce_gateway_ready(&handle, &gateway_config.host, gateway_config.port).await;

    // Blocks until the server shuts down.
    gateway
        .start()
        .await
        .map_err(|e| format!("Gateway error: {}", e))?;

    Ok(())
}

/// Set up the system tray icon and context menu.
#[cfg(not(mobile))] // tauri::tray/menu do not exist on mobile targets
fn setup_tray_and_menu(app: &mut tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder};
    use tauri::tray::TrayIconBuilder;

    let handle = app.handle();

    let show_i = MenuItemBuilder::new("Show Window")
        .id("show")
        .build(handle)?;
    let update_i = MenuItemBuilder::new("Check for Updates")
        .id("update")
        .build(handle)?;
    let quit_i = MenuItemBuilder::new("Quit").id("quit").build(handle)?;

    let tray_menu = MenuBuilder::new(handle)
        .item(&show_i)
        .separator()
        .item(&update_i)
        .separator()
        .item(&quit_i)
        .build()?;

    TrayIconBuilder::new()
        .icon(
            handle
                .default_window_icon()
                .ok_or("No default window icon")?
                .clone(),
        )
        .menu(&tray_menu)
        .on_menu_event(|app: &tauri::AppHandle, event| match event.id().as_ref() {
            "show" => {
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
            "update" => {
                let handle = app.clone();
                tauri::async_runtime::spawn(async move {
                    if let Err(e) = run_update_check(handle).await {
                        eprintln!("Update check failed: {}", e);
                    }
                });
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            use tauri::tray::{MouseButton, MouseButtonState, TrayIconEvent};
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            }
        })
        .build(handle)?;

    Ok(())
}
