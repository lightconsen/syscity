//! Telegram Channel Extension
//!
//! Implements `ChannelExtension` for Telegram, bridging the native
//! `TelegramChannel` into the inbound/outbound pipeline architecture.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{mpsc, RwLock};
use tracing::warn;

use crate::channels::{Channel, ChannelExtension, IncomingMessage, OutgoingMessage};

/// Telegram channel extension.
///
/// Wraps a `TelegramChannel` and wires it into the Inbound/Outbound
/// pipelines via the `ChannelExtension` trait.
pub struct TelegramChannelExtension {
    channel: Arc<crate::channels::telegram::TelegramChannel>,
    session_channels: Arc<RwLock<HashMap<String, (String, String)>>>,
    /// The `[channels.<name>]` section this extension was built for.
    ///
    /// The session→channel map and the router's channel defaults are keyed by
    /// this name, not by the literal `"telegram"` type, so two Telegram bots
    /// configured under different section names do not overwrite each other's
    /// sessions or replies.
    instance: String,
}

impl TelegramChannelExtension {
    /// Create a new Telegram channel extension.
    pub fn new(
        channel: Arc<crate::channels::telegram::TelegramChannel>,
        session_channels: Arc<RwLock<HashMap<String, (String, String)>>>,
    ) -> Self {
        Self {
            channel,
            session_channels,
            instance: "telegram".to_string(),
        }
    }

    /// As [`new`](Self::new), for a channel configured under a specific
    /// `[channels.<name>]` section (see field docs on `instance`).
    pub fn with_instance(
        channel: Arc<crate::channels::telegram::TelegramChannel>,
        session_channels: Arc<RwLock<HashMap<String, (String, String)>>>,
        instance: impl Into<String>,
    ) -> Self {
        Self {
            channel,
            session_channels,
            instance: instance.into(),
        }
    }
}

#[async_trait::async_trait]
impl ChannelExtension for TelegramChannelExtension {
    fn name(&self) -> &str {
        "telegram"
    }

    async fn run_inbound(&self, inbound_tx: mpsc::Sender<IncomingMessage>) -> crate::Result<()> {
        // Create an internal unbounded channel to receive messages
        // from the TelegramChannel's update handler.
        let (internal_tx, mut internal_rx) = mpsc::unbounded_channel::<IncomingMessage>();

        // Set the channel's message sender so updates flow into our bridge.
        self.channel.set_message_sender(internal_tx).await;

        let session_channels = self.session_channels.clone();
        let instance = self.instance.clone();

        // Bridge task: forward Telegram messages to the inbound pipeline,
        // storing session mapping for reply routing.
        let bridge_handle = tokio::spawn(async move {
            while let Some(msg) = internal_rx.recv().await {
                let session_id = msg.conversation_id.0.clone();
                let chat_id = msg
                    .metadata
                    .extra
                    .get("telegram_chat_id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or_else(|| session_id.parse().unwrap_or(0));

                // Store session -> (channel, chat_id) mapping for response
                // routing. Keyed by the configured instance so two bots do not
                // share a session table.
                {
                    let mut sessions = session_channels.write().await;
                    sessions.insert(session_id, (instance.clone(), chat_id.to_string()));
                }

                // Tell the router which instance this arrived on; the channel
                // default is registered under the same name.
                let msg = msg.with_channel_instance(instance.clone());
                if inbound_tx.send(msg).await.is_err() {
                    break;
                }
            }
        });

        // Start the Telegram bot dispatcher.  This blocks until the
        // channel is explicitly stopped.
        let result = self.channel.start().await;

        // Clean up the bridge when the channel stops.
        bridge_handle.abort();
        result
    }

    async fn run_outbound(
        &self,
        mut outbound_rx: mpsc::Receiver<OutgoingMessage>,
    ) -> crate::Result<()> {
        while let Some(msg) = outbound_rx.recv().await {
            if let Err(e) = self.channel.send(msg).await {
                warn!("Failed to send Telegram message via extension: {}", e);
            }
        }
        Ok(())
    }
}
