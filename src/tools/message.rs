//! Message delivery tool
//!
//! Send messages, replies, edits, deletes, and typing indicators through
//! channels via direct Channel trait method calls (Path B).  This is a
//! side-effect operation: it does NOT loop back through the inbound pipeline.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::Value;

use super::{Tool, ToolContext, ToolExecutionResult};
use crate::channels::{Channel, ConversationId, MessageOptions, OutgoingMessage};
use crate::core::models::Id;
use crate::gateway::GatewayState;
use crate::tools::sdk::ToolCapabilities;

/// Send a message through a channel.
pub struct MessageTool {
    state: Arc<GatewayState>,
}

impl MessageTool {
    pub fn new(state: Arc<GatewayState>) -> Self {
        Self { state }
    }

    /// Look up a channel by name from the gateway state.
    async fn resolve_channel(&self, name: &str) -> Option<Arc<dyn Channel>> {
        let channels = self.state.channels.channels.read().await;
        channels.get(name).cloned()
    }
}

#[derive(Debug, Deserialize)]
struct MessageArgs {
    action: String,
    channel: String,
    conversation_id: String,
    #[serde(default)]
    content: String,
    #[serde(default)]
    message_id: Option<String>,
    #[serde(default)]
    emoji: Option<String>,
    #[serde(default)]
    poll_question: Option<String>,
    #[serde(default)]
    poll_options: Option<Vec<String>>,
    #[serde(default)]
    thread_title: Option<String>,
    #[serde(default)]
    options: Option<MessageActionOptions>,
}

#[derive(Debug, Deserialize, Default)]
struct MessageActionOptions {
    #[serde(default)]
    silent: bool,
    #[serde(default)]
    show_typing: bool,
}

#[async_trait]
impl Tool for MessageTool {
    fn name(&self) -> &str {
        "message"
    }

    fn description(&self) -> &str {
        "Send messages and perform message actions through a channel (e.g., telegram, discord). \
         Actions: send, reply, edit, delete, typing, react, unreact, pin, unpin, thread_create, \
         poll. This is a side-effect operation that does not re-trigger the agent."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["send", "reply", "edit", "delete", "typing", "react", "unreact", "pin", "unpin", "thread_create", "poll"],
                    "description": "Message action to perform"
                },
                "channel": {
                    "type": "string",
                    "description": "Channel name (e.g., 'telegram', 'discord', 'web')"
                },
                "conversation_id": {
                    "type": "string",
                    "description": "Conversation/thread ID to target"
                },
                "content": {
                    "type": "string",
                    "description": "Message content (required for send, reply, edit)"
                },
                "message_id": {
                    "type": "string",
                    "description": "Internal message ID (required for reply, edit, delete, react, unreact, pin, unpin, thread_create)"
                },
                "emoji": {
                    "type": "string",
                    "description": "Emoji for react/unreact actions"
                },
                "poll_question": {
                    "type": "string",
                    "description": "Poll question (required for poll action)"
                },
                "poll_options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Poll options (required for poll action)"
                },
                "thread_title": {
                    "type": "string",
                    "description": "Thread title (optional for thread_create)"
                },
                "options": {
                    "type": "object",
                    "properties": {
                        "silent": { "type": "boolean", "description": "Send without notification" },
                        "show_typing": { "type": "boolean", "description": "Show typing indicator first" }
                    }
                }
            },
            "required": ["action", "channel", "conversation_id"]
        })
    }

    fn capabilities(&self) -> ToolCapabilities {
        ToolCapabilities {
            requires_approval: false,
            risk_level: crate::tools::approval::RiskLevel::Medium,
            categories: vec!["communication".to_string()],
            // Sent is sent: there is nothing to compensate with, and the
            // retry a caller might reach for would send it twice.
            idempotent: false,
            compensation: None,
            ..Default::default()
        }
    }

    async fn execute(
        &self,
        args: Value,
        _context: &ToolContext,
    ) -> crate::Result<ToolExecutionResult> {
        let start = std::time::Instant::now();
        let args: MessageArgs = match serde_json::from_value(args) {
            Ok(a) => a,
            Err(e) => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Invalid arguments: {}", e)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        let channel = match self.resolve_channel(&args.channel).await {
            Some(c) => c,
            None => {
                return Ok(ToolExecutionResult {
                    success: false,
                    output: String::new(),
                    error: Some(format!("Channel '{}' not found", args.channel)),
                    data: None,
                    execution_time: start.elapsed(),
                });
            }
        };

        let caps = channel.capabilities();
        let conversation_id = ConversationId::new(&args.conversation_id);

        match args.action.as_str() {
            "send" => {
                if args.content.is_empty() {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some("content is required for send action".to_string()),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                let options = args
                    .options
                    .map(|o| MessageOptions {
                        silent: o.silent,
                        show_typing: o.show_typing,
                        ..Default::default()
                    })
                    .unwrap_or_default();

                let msg =
                    OutgoingMessage::new(conversation_id, &args.content).with_options(options);

                match channel.send(msg).await {
                    Ok(id) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Message sent to {} (id: {})", args.channel, id),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "message_id": id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to send message: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "reply" => {
                if args.content.is_empty() {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some("content is required for reply action".to_string()),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                let reply_to_id = match args.message_id {
                    Some(id_str) => match Id::parse(&id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid message_id: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    },
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("message_id is required for reply action".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                let options = args
                    .options
                    .map(|o| MessageOptions {
                        silent: o.silent,
                        show_typing: o.show_typing,
                        ..Default::default()
                    })
                    .unwrap_or_default();

                let msg = OutgoingMessage::new(conversation_id, &args.content)
                    .reply_to(reply_to_id)
                    .with_options(options);

                match channel.send(msg).await {
                    Ok(id) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Reply sent to {} (id: {})", args.channel, id),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "message_id": id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to send reply: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "edit" => {
                if !caps.supports_edit {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Channel '{}' does not support message editing",
                            args.channel
                        )),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                if args.content.is_empty() {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some("content is required for edit action".to_string()),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                let message_id = match args.message_id {
                    Some(id_str) => match Id::parse(&id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid message_id: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    },
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("message_id is required for edit action".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                match channel.edit_message(message_id, args.content).await {
                    Ok(()) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Message edited in {}", args.channel),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "message_id": message_id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to edit message: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "delete" => {
                if !caps.supports_unsend {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Channel '{}' does not support message deletion",
                            args.channel
                        )),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                let message_id = match args.message_id {
                    Some(id_str) => match Id::parse(&id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid message_id: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    },
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("message_id is required for delete action".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                match channel.delete_message(message_id).await {
                    Ok(()) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Message deleted in {}", args.channel),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "message_id": message_id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to delete message: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "typing" => {
                if !caps.supports_typing {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Channel '{}' does not support typing indicators",
                            args.channel
                        )),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                match channel.send_typing(&conversation_id).await {
                    Ok(()) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Typing indicator sent to {}", args.channel),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "conversation_id": conversation_id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to send typing indicator: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "react" => {
                if !caps.supports_reactions {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Channel '{}' does not support reactions",
                            args.channel
                        )),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                let message_id = match args.message_id {
                    Some(id_str) => match Id::parse(&id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid message_id: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    },
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("message_id is required for react action".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                let emoji = match args.emoji {
                    Some(e) => e,
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("emoji is required for react action".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                match channel.add_reaction(message_id, emoji).await {
                    Ok(()) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Reaction added in {}", args.channel),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "message_id": message_id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to add reaction: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "unreact" => {
                if !caps.supports_reactions {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!(
                            "Channel '{}' does not support reactions",
                            args.channel
                        )),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                let message_id = match args.message_id {
                    Some(id_str) => match Id::parse(&id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid message_id: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    },
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("message_id is required for unreact action".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                let emoji = match args.emoji {
                    Some(e) => e,
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("emoji is required for unreact action".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                match channel.remove_reaction(message_id, emoji).await {
                    Ok(()) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Reaction removed in {}", args.channel),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "message_id": message_id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to remove reaction: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "pin" => {
                let message_id = match args.message_id {
                    Some(id_str) => match Id::parse(&id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid message_id: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    },
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("message_id is required for pin action".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                match channel.pin_message(message_id).await {
                    Ok(()) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Message pinned in {}", args.channel),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "message_id": message_id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to pin message: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "unpin" => {
                let message_id = match args.message_id {
                    Some(id_str) => match Id::parse(&id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid message_id: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    },
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("message_id is required for unpin action".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                match channel.unpin_message(message_id).await {
                    Ok(()) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Message unpinned in {}", args.channel),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "message_id": message_id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to unpin message: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "thread_create" => {
                if !caps.supports_threads {
                    return Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Channel '{}' does not support threads", args.channel)),
                        data: None,
                        execution_time: start.elapsed(),
                    });
                }

                let message_id = match args.message_id {
                    Some(id_str) => match Id::parse(&id_str) {
                        Ok(id) => id,
                        Err(e) => {
                            return Ok(ToolExecutionResult {
                                success: false,
                                output: String::new(),
                                error: Some(format!("Invalid message_id: {}", e)),
                                data: None,
                                execution_time: start.elapsed(),
                            });
                        }
                    },
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "message_id is required for thread_create action".to_string(),
                            ),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                match channel.create_thread(message_id, args.thread_title).await {
                    Ok(thread_id) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Thread created in {} (id: {})", args.channel, thread_id),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "thread_id": thread_id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to create thread: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            "poll" => {
                let question = match args.poll_question {
                    Some(q) => q,
                    None => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some("poll_question is required for poll action".to_string()),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                let options = match args.poll_options {
                    Some(opts) if !opts.is_empty() => opts,
                    _ => {
                        return Ok(ToolExecutionResult {
                            success: false,
                            output: String::new(),
                            error: Some(
                                "poll_options is required for poll action (at least 1 option)"
                                    .to_string(),
                            ),
                            data: None,
                            execution_time: start.elapsed(),
                        });
                    }
                };

                match channel.send_poll(conversation_id, question, options).await {
                    Ok(id) => Ok(ToolExecutionResult {
                        success: true,
                        output: format!("Poll sent to {} (id: {})", args.channel, id),
                        error: None,
                        data: Some(serde_json::json!({
                            "channel": args.channel,
                            "message_id": id.to_string(),
                        })),
                        execution_time: start.elapsed(),
                    }),
                    Err(e) => Ok(ToolExecutionResult {
                        success: false,
                        output: String::new(),
                        error: Some(format!("Failed to send poll: {}", e)),
                        data: None,
                        execution_time: start.elapsed(),
                    }),
                }
            }

            _ => Ok(ToolExecutionResult {
                success: false,
                output: String::new(),
                error: Some(format!("Unknown message action: {}", args.action)),
                data: None,
                execution_time: start.elapsed(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_message_args_send_parsing() {
        let args: MessageArgs = serde_json::from_value(serde_json::json!({
            "action": "send",
            "channel": "telegram",
            "conversation_id": "chat123",
            "content": "hello"
        }))
        .unwrap();
        assert_eq!(args.action, "send");
        assert_eq!(args.channel, "telegram");
        assert_eq!(args.conversation_id, "chat123");
        assert_eq!(args.content, "hello");
        assert!(args.message_id.is_none());
    }

    #[test]
    fn test_message_args_reply_parsing() {
        let args: MessageArgs = serde_json::from_value(serde_json::json!({
            "action": "reply",
            "channel": "discord",
            "conversation_id": "thread456",
            "content": "reply text",
            "message_id": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .unwrap();
        assert_eq!(args.action, "reply");
        assert_eq!(args.channel, "discord");
        assert_eq!(args.conversation_id, "thread456");
        assert_eq!(args.content, "reply text");
        assert_eq!(args.message_id, Some("550e8400-e29b-41d4-a716-446655440000".to_string()));
    }

    #[test]
    fn test_message_args_edit_parsing() {
        let args: MessageArgs = serde_json::from_value(serde_json::json!({
            "action": "edit",
            "channel": "telegram",
            "conversation_id": "chat123",
            "content": "edited text",
            "message_id": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .unwrap();
        assert_eq!(args.action, "edit");
        assert_eq!(args.content, "edited text");
    }

    #[test]
    fn test_message_args_delete_parsing() {
        let args: MessageArgs = serde_json::from_value(serde_json::json!({
            "action": "delete",
            "channel": "discord",
            "conversation_id": "thread456",
            "message_id": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .unwrap();
        assert_eq!(args.action, "delete");
    }

    #[test]
    fn test_message_args_typing_parsing() {
        let args: MessageArgs = serde_json::from_value(serde_json::json!({
            "action": "typing",
            "channel": "web",
            "conversation_id": "session789"
        }))
        .unwrap();
        assert_eq!(args.action, "typing");
        assert_eq!(args.channel, "web");
        assert_eq!(args.conversation_id, "session789");
    }

    #[test]
    fn test_message_args_with_options() {
        let args: MessageArgs = serde_json::from_value(serde_json::json!({
            "action": "send",
            "channel": "telegram",
            "conversation_id": "chat123",
            "content": "hello",
            "options": {
                "silent": true,
                "show_typing": true
            }
        }))
        .unwrap();
        assert!(args.options.is_some());
        let opts = args.options.unwrap();
        assert!(opts.silent);
        assert!(opts.show_typing);
    }

    #[test]
    fn test_message_args_react_parsing() {
        let args: MessageArgs = serde_json::from_value(serde_json::json!({
            "action": "react",
            "channel": "discord",
            "conversation_id": "thread456",
            "message_id": "550e8400-e29b-41d4-a716-446655440000",
            "emoji": "👍"
        }))
        .unwrap();
        assert_eq!(args.action, "react");
        assert_eq!(args.emoji, Some("👍".to_string()));
    }

    #[test]
    fn test_message_args_poll_parsing() {
        let args: MessageArgs = serde_json::from_value(serde_json::json!({
            "action": "poll",
            "channel": "telegram",
            "conversation_id": "chat123",
            "poll_question": "What's your favorite color?",
            "poll_options": ["Red", "Green", "Blue"]
        }))
        .unwrap();
        assert_eq!(args.action, "poll");
        assert_eq!(args.poll_question, Some("What's your favorite color?".to_string()));
        assert_eq!(
            args.poll_options,
            Some(vec!["Red".to_string(), "Green".to_string(), "Blue".to_string()])
        );
    }

    #[test]
    fn test_message_args_thread_create_parsing() {
        let args: MessageArgs = serde_json::from_value(serde_json::json!({
            "action": "thread_create",
            "channel": "discord",
            "conversation_id": "thread456",
            "message_id": "550e8400-e29b-41d4-a716-446655440000",
            "thread_title": "Discussion thread"
        }))
        .unwrap();
        assert_eq!(args.action, "thread_create");
        assert_eq!(args.thread_title, Some("Discussion thread".to_string()));
    }
}
