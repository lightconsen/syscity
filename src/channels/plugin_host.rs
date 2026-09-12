//! WASM Plugin Host for Channel Extensions
//!
//! This module provides the runtime for loading and executing WASM-based
//! channel plugins, enabling third-party channels without recompiling Syscity.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, Mutex, RwLock};
use tracing::{debug, info, warn};
use wasmtime::{Engine, Linker, Module, Store, TypedFunc};

use crate::channels::{
    Channel, ChannelCapabilities, ConversationId, Id, IncomingMessage, MessageMetadata,
    OutgoingMessage, UserId,
};

/// Host state passed to WASM plugins
pub struct HostState {
    /// Channel name
    pub name: String,
    /// Configuration JSON
    pub config: String,
    /// Message sender for incoming messages
    pub message_tx: mpsc::UnboundedSender<IncomingMessage>,
}

/// A WASM-based channel plugin
#[allow(dead_code)]
pub struct PluginChannel {
    /// Plugin name
    name: String,
    /// Plugin ID
    id: String,
    /// Cached capabilities (fetched during load)
    capabilities: ChannelCapabilities,
    /// WASM store
    store: Arc<Mutex<Store<HostState>>>,
    /// Function pointers
    init_fn: TypedFunc<(i32, i32), i64>,
    start_fn: TypedFunc<(), i32>,
    stop_fn: TypedFunc<(), i32>,
    get_name_fn: TypedFunc<(), (i32, i32)>,
    get_capabilities_fn: TypedFunc<(), i64>,
    send_fn: TypedFunc<(i32, i32), i64>,
    send_typing_fn: TypedFunc<(i32, i32), i32>,
    edit_message_fn: TypedFunc<(i32, i32, i32, i32), i32>,
    delete_message_fn: TypedFunc<(i32, i32), i32>,
    health_check_fn: TypedFunc<(), i32>,
    /// Memory export for reading/writing strings
    memory: wasmtime::Memory,
    /// Alloc function for allocating memory in the guest
    alloc_fn: TypedFunc<i32, i32>,
    /// Free function for freeing memory in the guest
    free_fn: TypedFunc<(i32, i32), ()>,
}

/// Maximum allowed buffer size for WASM memory operations (1MB)
const MAX_WASM_BUFFER_SIZE: usize = 1024 * 1024;

/// Helper to write a string to WASM memory and return (ptr, len)
fn write_string_to_memory(
    store: &mut Store<HostState>,
    memory: &wasmtime::Memory,
    alloc_fn: &TypedFunc<i32, i32>,
    s: &str,
) -> crate::Result<(i32, i32)> {
    let bytes = s.as_bytes();
    let len = bytes.len();

    // Check buffer size limit
    if len > MAX_WASM_BUFFER_SIZE {
        return Err(crate::error::SyscityError::Plugin(format!(
            "String too large: {} bytes (max: {})",
            len, MAX_WASM_BUFFER_SIZE
        )));
    }

    let len_i32 = len as i32;

    // Allocate memory in the guest
    let ptr = alloc_fn.call(&mut *store, len_i32).map_err(|e| {
        crate::error::SyscityError::Plugin(format!("Failed to allocate memory: {}", e))
    })?;

    // Validate pointer
    if ptr < 0 {
        return Err(crate::error::SyscityError::Plugin(
            "Allocator returned invalid pointer".to_string(),
        ));
    }

    // Check memory bounds
    let mem_size = memory.data_size(&*store);
    if (ptr as usize + len) > mem_size {
        return Err(crate::error::SyscityError::Plugin(
            "Allocated memory out of bounds".to_string(),
        ));
    }

    // Write the string
    memory.write(store, ptr as usize, bytes).map_err(|e| {
        crate::error::SyscityError::Plugin(format!("Failed to write to memory: {}", e))
    })?;

    Ok((ptr, len_i32))
}

/// Helper to read a string from WASM memory
fn read_string_from_memory(
    store: &mut Store<HostState>,
    memory: &wasmtime::Memory,
    ptr: i32,
    len: i32,
) -> crate::Result<String> {
    // Validate parameters
    if ptr < 0 || len < 0 {
        return Err(crate::error::SyscityError::Plugin("Invalid pointer or length".to_string()));
    }

    let len = len as usize;

    // Check buffer size limit
    if len > MAX_WASM_BUFFER_SIZE {
        return Err(crate::error::SyscityError::Plugin(format!(
            "Buffer too large: {} bytes (max: {})",
            len, MAX_WASM_BUFFER_SIZE
        )));
    }

    // Check memory bounds
    let mem_size = memory.data_size(&*store);
    if (ptr as usize + len) > mem_size {
        return Err(crate::error::SyscityError::Plugin("Read out of bounds".to_string()));
    }

    let mut buffer = vec![0u8; len];
    memory
        .read(&mut *store, ptr as usize, &mut buffer)
        .map_err(|e| {
            crate::error::SyscityError::Plugin(format!("Failed to read from memory: {}", e))
        })?;

    String::from_utf8(buffer)
        .map_err(|e| crate::error::SyscityError::Plugin(format!("Invalid UTF-8: {}", e)))
}

impl PluginChannel {
    /// Load a WASM plugin from file
    pub async fn load(
        wasm_path: &std::path::Path,
        config: serde_json::Value,
        message_tx: mpsc::UnboundedSender<IncomingMessage>,
    ) -> crate::Result<Self> {
        debug!("Loading WASM plugin from {:?}", wasm_path);

        // Read WASM bytes
        let wasm_bytes =
            tokio::fs::read(wasm_path)
                .await
                .map_err(|e| crate::error::SyscityError::Storage {
                    context: format!("Failed to read WASM file: {:?}", wasm_path),
                    details: e.to_string(),
                })?;

        let config_str = config.to_string();
        let plugin_name = wasm_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let plugin_name_for_spawn = plugin_name.clone();

        // Use spawn_blocking for WASM operations
        let (mut store, instance, memory, alloc_fn, free_fn) = tokio::task::spawn_blocking(move || {
            let engine = Engine::default();
            let module = Module::new(&engine, &wasm_bytes).map_err(|e| {
                crate::error::SyscityError::Plugin(format!("Failed to compile WASM: {}", e))
            })?;

            // Create linker with host functions
            let mut linker: Linker<HostState> = Linker::new(&engine);

            // Define host.log function
            linker.func_wrap(
                "host",
                "log",
                |mut caller: wasmtime::Caller<'_, HostState>, level: i32, ptr: i32, len: i32| {
                    // Validate parameters
                    if ptr < 0 || len < 0 || len as usize > MAX_WASM_BUFFER_SIZE {
                        eprintln!("[WARN] Plugin log: invalid parameters ptr={}, len={}", ptr, len);
                        return;
                    }

                    if let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        // Check bounds
                        let mem_size = memory.data_size(&caller);
                        if (ptr as usize + len as usize) > mem_size {
                            eprintln!("[WARN] Plugin log: memory out of bounds");
                            return;
                        }

                        let mut buffer = vec![0u8; len as usize];
                        match memory.read(&caller, ptr as usize, &mut buffer) {
                            Ok(_) => match String::from_utf8(buffer) {
                                Ok(message) => {
                                    let level_str = match level {
                                        0 => "DEBUG",
                                        1 => "INFO",
                                        2 => "WARN",
                                        3 => "ERROR",
                                        _ => "UNKNOWN",
                                    };
                                    println!("[{}] {}", level_str, message);
                                }
                                Err(e) => eprintln!("[WARN] Plugin log: invalid UTF-8: {}", e),
                            },
                            Err(e) => eprintln!("[WARN] Plugin log: memory read failed: {}", e),
                        }
                    } else {
                        eprintln!("[WARN] Plugin log: no memory export");
                    }
                },
            ).map_err(|e| crate::error::SyscityError::Plugin(format!("Failed to define log: {}", e)))?;

            // Define host.receive_message function
            linker.func_wrap(
                "host",
                "receive-message",
                |mut caller: wasmtime::Caller<'_, HostState>, ptr: i32, len: i32| {
                    // Validate parameters
                    if ptr < 0 || len < 0 || len as usize > MAX_WASM_BUFFER_SIZE {
                        eprintln!("[WARN] Plugin receive_message: invalid parameters ptr={}, len={}", ptr, len);
                        return;
                    }

                    if let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) {
                        // Check bounds
                        let mem_size = memory.data_size(&caller);
                        if (ptr as usize + len as usize) > mem_size {
                            eprintln!("[WARN] Plugin receive_message: memory out of bounds");
                            return;
                        }

                        let mut buffer = vec![0u8; len as usize];
                        match memory.read(&caller, ptr as usize, &mut buffer) {
                            Ok(_) => match String::from_utf8(buffer) {
                                Ok(json) => match serde_json::from_str::<serde_json::Value>(&json) {
                                    Ok(msg) => {
                                        // Send to host's message channel
                                        let tx = caller.data().message_tx.clone();
                                        // Parse incoming message from JSON
                                        let incoming = IncomingMessage {
                                                id: Id::new(),
                                                user_id: UserId::new(
                                                    msg.get("user_id")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("unknown"),
                                                ),
                                                conversation_id: ConversationId::new(
                                                    msg.get("conversation_id")
                                                        .and_then(|v| v.as_str())
                                                        .unwrap_or("default"),
                                                ),
                                                content: msg
                                                    .get("content")
                                                    .and_then(|v| v.as_str())
                                                    .unwrap_or("")
                                                    .to_string(),
                                                attachments: vec![],
                                                metadata: MessageMetadata::new(),
                                                provenance: crate::channels::InputProvenance::ExternalUser {
                                                    channel: "plugin".to_string(),
                                                    is_direct: true,
                                                },
                                                mention: crate::channels::MentionState::DirectMessage,
                                            };
                                        if let Err(e) = tx.send(incoming) {
                                            eprintln!("[WARN] Plugin receive_message: failed to send: {}", e);
                                        }
                                    }
                                    Err(e) => eprintln!("[WARN] Plugin receive_message: invalid JSON: {}", e),
                                },
                                Err(e) => eprintln!("[WARN] Plugin receive_message: invalid UTF-8: {}", e),
                            },
                            Err(e) => eprintln!("[WARN] Plugin receive_message: memory read failed: {}", e),
                        }
                    } else {
                        eprintln!("[WARN] Plugin receive_message: no memory export");
                    }
                },
            ).map_err(|e| crate::error::SyscityError::Plugin(format!("Failed to define receive_message: {}", e)))?;

            // Define host.get_config function - returns packed i64: high 32 = len, low 32 = ptr
            linker.func_wrap(
                "host",
                "get-config",
                |mut caller: wasmtime::Caller<'_, HostState>| -> i64 {
                    let config_bytes = caller.data().config.as_bytes().to_vec();
                    let len = config_bytes.len() as i32;

                    // Obtain alloc function from guest (Func is Copy — no borrow retained)
                    let alloc_fn = caller
                        .get_export("alloc")
                        .and_then(|e| e.into_func());
                    let alloc_typed = alloc_fn
                        .and_then(|f| f.typed::<i32, i32>(&caller).ok());

                    // Call guest alloc to reserve a buffer
                    let ptr = match alloc_typed {
                        Some(f) => match f.call(&mut caller, len) {
                            Ok(p) => p,
                            Err(_) => return 0i64,
                        },
                        None => return 0i64,
                    };

                    // Write config bytes into guest linear memory
                    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
                        Some(m) => m,
                        None => return 0i64,
                    };
                    let data = memory.data_mut(&mut caller);
                    let start = ptr as usize;
                    let end = start + config_bytes.len();
                    if end <= data.len() {
                        data[start..end].copy_from_slice(&config_bytes);
                        ((len as i64) << 32) | (ptr as i64)
                    } else {
                        0i64
                    }
                },
            ).map_err(|e| crate::error::SyscityError::Plugin(format!("Failed to define get_config: {}", e)))?;

            let host_state = HostState {
                name: plugin_name_for_spawn,
                config: config_str,
                message_tx,
            };

            let mut store = Store::new(&engine, host_state);
            let instance = linker.instantiate(&mut store, &module).map_err(|e| {
                crate::error::SyscityError::Plugin(format!("Failed to instantiate: {}", e))
            })?;

            // Get memory export
            let memory = instance.get_export(&mut store, "memory")
                .and_then(|e| e.into_memory())
                .ok_or_else(|| crate::error::SyscityError::Plugin("No memory export".to_string()))?;

            // Get alloc function
            let alloc_fn = instance.get_typed_func::<i32, i32>(&mut store, "alloc"
            ).map_err(|e| crate::error::SyscityError::Plugin(format!("No alloc export: {}", e)))?;

            // Get free function
            let free_fn = instance.get_typed_func::<(i32, i32), ()>(
                &mut store, "free"
            ).map_err(|e| crate::error::SyscityError::Plugin(format!("No free export: {}", e)))?;

            Ok::<_, crate::error::SyscityError>((store, instance, memory, alloc_fn, free_fn))
        }).await.map_err(|e| crate::error::SyscityError::Plugin(format!("Task failed: {}", e)))??;

        // Get function pointers
        let (
            init_fn,
            start_fn,
            stop_fn,
            get_name_fn,
            get_capabilities_fn,
            send_fn,
            send_typing_fn,
            edit_message_fn,
            delete_message_fn,
            health_check_fn,
        ) = {
            let init_fn = instance
                .get_typed_func::<(i32, i32), i64>(&mut store, "init")
                .map_err(|e| crate::error::SyscityError::Plugin(format!("No init: {}", e)))?;

            let start_fn = instance
                .get_typed_func::<(), i32>(&mut store, "start")
                .map_err(|e| crate::error::SyscityError::Plugin(format!("No start: {}", e)))?;

            let stop_fn = instance
                .get_typed_func::<(), i32>(&mut store, "stop")
                .map_err(|e| crate::error::SyscityError::Plugin(format!("No stop: {}", e)))?;

            let get_name_fn = instance
                .get_typed_func::<(), (i32, i32)>(&mut store, "get_name")
                .map_err(|e| crate::error::SyscityError::Plugin(format!("No get_name: {}", e)))?;

            let get_capabilities_fn = instance
                .get_typed_func::<(), i64>(&mut store, "get_capabilities")
                .map_err(|e| {
                    crate::error::SyscityError::Plugin(format!("No get_capabilities: {}", e))
                })?;

            let send_fn = instance
                .get_typed_func::<(i32, i32), i64>(&mut store, "send")
                .map_err(|e| crate::error::SyscityError::Plugin(format!("No send: {}", e)))?;

            let send_typing_fn = instance
                .get_typed_func::<(i32, i32), i32>(&mut store, "send_typing")
                .map_err(|e| {
                    crate::error::SyscityError::Plugin(format!("No send_typing: {}", e))
                })?;

            let edit_message_fn = instance
                .get_typed_func::<(i32, i32, i32, i32), i32>(&mut store, "edit_message")
                .map_err(|e| {
                    crate::error::SyscityError::Plugin(format!("No edit_message: {}", e))
                })?;

            let delete_message_fn = instance
                .get_typed_func::<(i32, i32), i32>(&mut store, "delete_message")
                .map_err(|e| {
                    crate::error::SyscityError::Plugin(format!("No delete_message: {}", e))
                })?;

            let health_check_fn = instance
                .get_typed_func::<(), i32>(&mut store, "health_check")
                .map_err(|e| {
                    crate::error::SyscityError::Plugin(format!("No health_check: {}", e))
                })?;

            (
                init_fn,
                start_fn,
                stop_fn,
                get_name_fn,
                get_capabilities_fn,
                send_fn,
                send_typing_fn,
                edit_message_fn,
                delete_message_fn,
                health_check_fn,
            )
        };

        // Fetch capabilities from the plugin
        let capabilities = {
            let result = get_capabilities_fn.call(&mut store, ()).map_err(|e| {
                crate::error::SyscityError::Plugin(format!("Get capabilities failed: {}", e))
            })?;

            // Decode result: high 32 bits = ptr, low 32 bits = len (or error indicator)
            let result_ptr = (result >> 32) as i32;
            let result_len = (result & 0xFFFFFFFF) as i32;

            if result_ptr == 0 && result_len == 0 {
                // No capabilities returned, use defaults
                ChannelCapabilities {
                    chat_types: vec![crate::channels::ChatType::Direct],
                    supports_formatting: true,
                    supports_attachments: false,
                    supports_images: false,
                    supports_threads: false,
                    supports_typing: true,
                    supports_buttons: false,
                    supports_commands: false,
                    supports_reactions: false,
                    supports_edit: false,
                    supports_unsend: false,
                    supports_effects: false,
                }
            } else {
                // Read capabilities JSON from memory
                let caps_json =
                    read_string_from_memory(&mut store, &memory, result_ptr, result_len)?;

                // Free the memory
                free_fn
                    .call(&mut store, (result_ptr, result_len))
                    .map_err(|e| {
                        crate::error::SyscityError::Plugin(format!("Free failed: {}", e))
                    })?;

                // Parse JSON
                match serde_json::from_str::<CapabilitiesJson>(&caps_json) {
                    Ok(caps) => caps.to_capabilities(),
                    Err(_) => ChannelCapabilities {
                        chat_types: vec![crate::channels::ChatType::Direct],
                        supports_formatting: true,
                        supports_attachments: false,
                        supports_images: false,
                        supports_threads: false,
                        supports_typing: true,
                        supports_buttons: false,
                        supports_commands: false,
                        supports_reactions: false,
                        supports_edit: false,
                        supports_unsend: false,
                        supports_effects: false,
                    },
                }
            }
        };

        let plugin_id = uuid::Uuid::new_v4().to_string();

        info!(
            "Loaded WASM plugin '{}' (ID: {}) with capabilities: {:?}",
            plugin_name, plugin_id, capabilities.chat_types
        );

        Ok(Self {
            name: plugin_name,
            id: plugin_id,
            capabilities,
            store: Arc::new(Mutex::new(store)),
            init_fn,
            start_fn,
            stop_fn,
            get_name_fn,
            get_capabilities_fn,
            send_fn,
            send_typing_fn,
            edit_message_fn,
            delete_message_fn,
            health_check_fn,
            memory,
            alloc_fn,
            free_fn,
        })
    }

    /// Initialize the plugin with configuration
    pub async fn init(&self, config: &serde_json::Value) -> crate::Result<String> {
        let mut store = self.store.lock().await;
        let (ptr, len) =
            write_string_to_memory(&mut store, &self.memory, &self.alloc_fn, &config.to_string())?;

        let result = self
            .init_fn
            .call(&mut *store, (ptr, len))
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Init failed: {}", e)))?;

        // Free the input string
        self.free_fn
            .call(&mut *store, (ptr, len))
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Free failed: {}", e)))?;

        // Parse result - simplified
        if result == 0 {
            Ok("initialized".to_string())
        } else {
            Err(crate::error::SyscityError::Plugin("Init returned error".to_string()))
        }
    }
}

/// Helper struct for deserializing capabilities from JSON
#[derive(serde::Deserialize)]
struct CapabilitiesJson {
    #[serde(rename = "chat_types", default)]
    chat_types: Vec<String>,
    #[serde(rename = "supports_formatting", default = "default_true")]
    supports_formatting: bool,
    #[serde(rename = "supports_attachments", default = "default_true")]
    supports_attachments: bool,
    #[serde(rename = "supports_images", default = "default_true")]
    supports_images: bool,
    #[serde(rename = "supports_threads", default)]
    supports_threads: bool,
    #[serde(rename = "supports_typing", default = "default_true")]
    supports_typing: bool,
    #[serde(rename = "supports_buttons", default)]
    supports_buttons: bool,
    #[serde(rename = "supports_commands", default)]
    supports_commands: bool,
    #[serde(rename = "supports_reactions", default)]
    supports_reactions: bool,
    #[serde(rename = "supports_edit", default)]
    supports_edit: bool,
    #[serde(rename = "supports_unsend", default)]
    supports_unsend: bool,
    #[serde(rename = "supports_effects", default)]
    supports_effects: bool,
}

fn default_true() -> bool {
    true
}

impl CapabilitiesJson {
    fn to_capabilities(&self) -> ChannelCapabilities {
        use crate::channels::ChatType;

        let chat_types = self
            .chat_types
            .iter()
            .filter_map(|s| match s.as_str() {
                "direct" => Some(ChatType::Direct),
                "group" => Some(ChatType::Group),
                "channel" => Some(ChatType::Channel),
                "thread" => Some(ChatType::Thread),
                _ => None,
            })
            .collect();

        ChannelCapabilities {
            chat_types,
            supports_formatting: self.supports_formatting,
            supports_attachments: self.supports_attachments,
            supports_images: self.supports_images,
            supports_threads: self.supports_threads,
            supports_typing: self.supports_typing,
            supports_buttons: self.supports_buttons,
            supports_commands: self.supports_commands,
            supports_reactions: self.supports_reactions,
            supports_edit: self.supports_edit,
            supports_unsend: self.supports_unsend,
            supports_effects: self.supports_effects,
        }
    }
}

#[async_trait]
impl Channel for PluginChannel {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> ChannelCapabilities {
        // Return cached capabilities that were fetched during plugin load
        self.capabilities.clone()
    }

    async fn start(&self) -> crate::Result<()> {
        let mut store = self.store.lock().await;
        let result = self
            .start_fn
            .call(&mut *store, ())
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Start failed: {}", e)))?;

        if result == 0 {
            info!("Started plugin channel '{}'", self.name);
            Ok(())
        } else {
            Err(crate::error::SyscityError::Plugin(format!("Start returned {}", result)))
        }
    }

    async fn stop(&self) -> crate::Result<()> {
        let mut store = self.store.lock().await;
        let result = self
            .stop_fn
            .call(&mut *store, ())
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Stop failed: {}", e)))?;

        if result == 0 {
            info!("Stopped plugin channel '{}'", self.name);
            Ok(())
        } else {
            Err(crate::error::SyscityError::Plugin(format!("Stop returned {}", result)))
        }
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let json = serde_json::json!({
            "conversation_id": message.conversation_id.to_string(),
            "content": message.content,
            "reply_to": message.reply_to.map(|r| r.to_string()),
        });

        let mut store = self.store.lock().await;
        let (ptr, len) =
            write_string_to_memory(&mut store, &self.memory, &self.alloc_fn, &json.to_string())?;

        let result = self
            .send_fn
            .call(&mut *store, (ptr, len))
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Send failed: {}", e)))?;

        self.free_fn
            .call(&mut *store, (ptr, len))
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Free failed: {}", e)))?;

        if result >= 0 {
            Ok(Id::new())
        } else {
            Err(crate::error::SyscityError::Plugin(format!("Send returned {}", result)))
        }
    }

    async fn send_typing(&self, conversation_id: &ConversationId) -> crate::Result<()> {
        let mut store = self.store.lock().await;
        let (ptr, len) = write_string_to_memory(
            &mut store,
            &self.memory,
            &self.alloc_fn,
            &conversation_id.to_string(),
        )?;

        let result = self
            .send_typing_fn
            .call(&mut *store, (ptr, len))
            .map_err(|e| {
                crate::error::SyscityError::Plugin(format!("Send_typing failed: {}", e))
            })?;

        self.free_fn
            .call(&mut *store, (ptr, len))
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Free failed: {}", e)))?;

        if result == 0 {
            Ok(())
        } else {
            Err(crate::error::SyscityError::Plugin(format!("Send_typing returned {}", result)))
        }
    }

    async fn edit_message(&self, message_id: Id, new_content: String) -> crate::Result<()> {
        let mut store = self.store.lock().await;
        let (ptr1, len1) = write_string_to_memory(
            &mut store,
            &self.memory,
            &self.alloc_fn,
            &message_id.to_string(),
        )?;
        let (ptr2, len2) =
            write_string_to_memory(&mut store, &self.memory, &self.alloc_fn, &new_content)?;

        let result = self
            .edit_message_fn
            .call(&mut *store, (ptr1, len1, ptr2, len2))
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Edit failed: {}", e)))?;

        self.free_fn
            .call(&mut *store, (ptr1, len1))
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Free failed: {}", e)))?;
        self.free_fn
            .call(&mut *store, (ptr2, len2))
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Free failed: {}", e)))?;

        if result == 0 {
            Ok(())
        } else {
            Err(crate::error::SyscityError::Plugin(format!("Edit returned {}", result)))
        }
    }

    async fn delete_message(&self, message_id: Id) -> crate::Result<()> {
        let mut store = self.store.lock().await;
        let (ptr, len) = write_string_to_memory(
            &mut store,
            &self.memory,
            &self.alloc_fn,
            &message_id.to_string(),
        )?;

        let result = self
            .delete_message_fn
            .call(&mut *store, (ptr, len))
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Delete failed: {}", e)))?;

        self.free_fn
            .call(&mut *store, (ptr, len))
            .map_err(|e| crate::error::SyscityError::Plugin(format!("Free failed: {}", e)))?;

        if result == 0 {
            Ok(())
        } else {
            Err(crate::error::SyscityError::Plugin(format!("Delete returned {}", result)))
        }
    }

    async fn health_check(&self) -> crate::Result<bool> {
        let mut store = self.store.lock().await;
        let result = self.health_check_fn.call(&mut *store, ()).map_err(|e| {
            crate::error::SyscityError::Plugin(format!("Health check failed: {}", e))
        })?;

        Ok(result == 1)
    }
}

/// Plugin manifest
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginManifest {
    pub name: String,
    pub version: String,
    pub author: String,
    pub description: String,
    pub capabilities: Vec<String>,
    pub config_schema: Option<serde_json::Value>,
}

/// Registry for managing WASM channel plugins
pub struct PluginChannelRegistry {
    plugins: Arc<RwLock<HashMap<String, Arc<PluginChannel>>>>,
    manifests: Arc<RwLock<HashMap<String, PluginManifest>>>,
    plugin_dir: PathBuf,
    message_tx: mpsc::UnboundedSender<IncomingMessage>,
}

impl PluginChannelRegistry {
    pub fn new(plugin_dir: PathBuf, message_tx: mpsc::UnboundedSender<IncomingMessage>) -> Self {
        Self {
            plugins: Arc::new(RwLock::new(HashMap::new())),
            manifests: Arc::new(RwLock::new(HashMap::new())),
            plugin_dir,
            message_tx,
        }
    }

    pub async fn discover_plugins(&self) -> crate::Result<Vec<(String, PathBuf)>> {
        let mut plugins = Vec::new();

        if !self.plugin_dir.exists() {
            return Ok(plugins);
        }

        let mut entries = tokio::fs::read_dir(&self.plugin_dir).await.map_err(|e| {
            crate::error::SyscityError::Storage {
                context: format!("Failed to read plugin directory: {:?}", self.plugin_dir),
                details: e.to_string(),
            }
        })?;

        while let Some(entry) =
            entries
                .next_entry()
                .await
                .map_err(|e| crate::error::SyscityError::Storage {
                    context: "Failed to read directory entry".to_string(),
                    details: e.to_string(),
                })?
        {
            let path = entry.path();
            if path.extension().map(|e| e == "wasm").unwrap_or(false) {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                plugins.push((name, path));
            }
        }

        Ok(plugins)
    }

    pub async fn load_plugin(
        &self,
        name: &str,
        config: Option<serde_json::Value>,
    ) -> crate::Result<Arc<PluginChannel>> {
        let wasm_path = self.plugin_dir.join(format!("{}.wasm", name));

        if !wasm_path.exists() {
            return Err(crate::error::SyscityError::NotFound {
                resource: format!("Plugin '{}'", name),
            });
        }

        {
            let plugins = self.plugins.read().await;
            if let Some(plugin) = plugins.get(name) {
                return Ok(plugin.clone());
            }
        }

        let manifest_path = self.plugin_dir.join(format!("{}.yaml", name));
        let (config, manifest) = if manifest_path.exists() {
            let manifest_yaml = tokio::fs::read_to_string(&manifest_path).await?;
            let m: PluginManifest = serde_norway::from_str(&manifest_yaml)?;
            let manifest = Some(m);
            (config.unwrap_or_else(|| serde_json::json!({})), manifest)
        } else {
            (config.unwrap_or_else(|| serde_json::json!({})), None)
        };

        let plugin =
            PluginChannel::load(&wasm_path, config.clone(), self.message_tx.clone()).await?;
        let _ = plugin.init(&config).await?;

        let plugin = Arc::new(plugin);

        {
            let mut plugins = self.plugins.write().await;
            plugins.insert(name.to_string(), plugin.clone());
        }

        // Store manifest if present
        if let Some(m) = manifest {
            let mut manifests = self.manifests.write().await;
            manifests.insert(name.to_string(), m);
        }

        info!("Loaded plugin '{}'", name);
        Ok(plugin)
    }

    pub async fn unload_plugin(&self, name: &str) -> crate::Result<()> {
        {
            let mut plugins = self.plugins.write().await;
            if let Some(plugin) = plugins.remove(name) {
                if let Err(e) = plugin.stop().await {
                    warn!("failed to stop plugin '{}': {e}", name);
                }
                info!("Unloaded plugin '{}'", name);
            }
        }
        {
            let mut manifests = self.manifests.write().await;
            manifests.remove(name);
        }
        Ok(())
    }

    pub async fn get_plugin(&self, name: &str) -> Option<Arc<PluginChannel>> {
        let plugins = self.plugins.read().await;
        plugins.get(name).cloned()
    }

    pub async fn get_manifest(&self, name: &str) -> Option<PluginManifest> {
        let manifests = self.manifests.read().await;
        manifests.get(name).cloned()
    }

    pub async fn list_loaded(&self) -> Vec<String> {
        let plugins = self.plugins.read().await;
        plugins.keys().cloned().collect()
    }

    pub async fn load_all(&self) -> crate::Result<Vec<String>> {
        let discovered = self.discover_plugins().await?;
        let mut loaded = Vec::new();

        for (name, _) in discovered {
            match self.load_plugin(&name, None).await {
                Ok(_) => loaded.push(name),
                Err(e) => warn!("Failed to load plugin '{}': {}", name, e),
            }
        }

        Ok(loaded)
    }

    pub async fn start_all(&self) -> Vec<crate::Result<()>> {
        let plugins = self.plugins.read().await;
        let mut results = Vec::new();

        for (name, plugin) in plugins.iter() {
            let result = plugin.start().await;
            if let Err(ref e) = result {
                warn!("Failed to start plugin '{}': {}", name, e);
            }
            results.push(result);
        }

        results
    }

    pub async fn stop_all(&self) -> Vec<crate::Result<()>> {
        let plugins = self.plugins.read().await;
        let mut results = Vec::new();

        for (name, plugin) in plugins.iter() {
            let result = plugin.stop().await;
            if let Err(ref e) = result {
                warn!("Failed to stop plugin '{}': {}", name, e);
            }
            results.push(result);
        }

        results
    }
}

impl Default for PluginChannelRegistry {
    fn default() -> Self {
        let (message_tx, _) = mpsc::unbounded_channel();
        Self {
            plugins: Arc::new(RwLock::new(HashMap::new())),
            manifests: Arc::new(RwLock::new(HashMap::new())),
            plugin_dir: PathBuf::from("./plugins"),
            message_tx,
        }
    }
}
