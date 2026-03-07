//! Telegram log: send log lines to a Telegram chat in a background task.
//! The main loop only enqueues messages (try_send, never blocks) so delay is unaffected.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

const TELEGRAM_QUEUE_CAP: usize = 128;
const TELEGRAM_MAX_TEXT_LEN: usize = 4000;

/// Sender handle for Telegram logs. Enqueue with `send()`; a background task does the HTTP.
#[derive(Clone)]
pub struct TelegramLog {
    tx: Option<mpsc::Sender<String>>,
}

impl TelegramLog {
    /// If token and chat_id are Some, spawns a background task that consumes messages
    /// and sends them via Telegram Bot API. Returns (TelegramLog, JoinHandle).
    /// The caller must keep the JoinHandle alive (e.g. store in state or await on shutdown).
    pub fn new(
        token: Option<String>,
        chat_id: Option<String>,
    ) -> (Self, Option<tokio::task::JoinHandle<()>>) {
        let (token, chat_id) = match (token, chat_id) {
            (Some(t), Some(c)) if !t.is_empty() && !c.is_empty() => (t, c),
            _ => return (Self { tx: None }, None),
        };

        let (tx, mut rx) = mpsc::channel::<String>(TELEGRAM_QUEUE_CAP);
        let token = Arc::new(token);
        let chat_id = Arc::new(chat_id);

        let handle = tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap_or_else(|_| {
                    reqwest::Client::builder()
                        .timeout(std::time::Duration::from_secs(15))
                        .build()
                        .unwrap_or_else(|_| reqwest::Client::new())
                });

            while let Some(text) = rx.recv().await {
                let text = if text.len() > TELEGRAM_MAX_TEXT_LEN {
                    format!("{}…", &text[..TELEGRAM_MAX_TEXT_LEN.saturating_sub(1)])
                } else {
                    text
                };
                if let Err(e) = send_message(&client, token.as_str(), chat_id.as_str(), &text).await
                {
                    tracing::warn!("[TelegramLog] send failed: {}", e);
                }
            }
        });

        (
            Self {
                tx: Some(tx),
            },
            Some(handle),
        )
    }

    /// Enqueue a message. Never blocks: uses try_send; if the queue is full the message is dropped.
    #[inline]
    pub fn send(&self, msg: impl AsRef<str>) {
        if let Some(ref tx) = self.tx {
            let s = msg.as_ref().to_string();
            if tx.try_send(s).is_err() {
                // Queue full; drop to avoid blocking. Optionally log once per burst.
                tracing::trace!("[TelegramLog] queue full, dropping message");
            }
        }
    }
}

async fn send_message(
    client: &reqwest::Client,
    token: &str,
    chat_id: &str,
    text: &str,
) -> Result<()> {
    let url = format!("https://api.telegram.org/bot{}/sendMessage", token);
    let res = client
        .post(&url)
        .timeout(std::time::Duration::from_secs(15))
        .form(&[("chat_id", chat_id), ("text", text)])
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        let body = res.text().await.unwrap_or_default();
        anyhow::bail!("Telegram API error {}: {}", status, body);
    }
    Ok(())
}
