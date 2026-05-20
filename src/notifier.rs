use reqwest::multipart::{Form, Part};
use reqwest::Client;
use tracing::warn;

#[derive(Clone)]
pub struct Notifier {
    webhook: String,
    client: Client,
}

impl Notifier {
    pub fn new(webhook: String, client: Client) -> Self {
        Self { webhook, client }
    }

    pub fn is_enabled(&self) -> bool {
        self.webhook.starts_with("https://discord.com/api/webhooks/")
    }

    /// Fire-and-forget. Spawns a tokio task; failures get logged.
    pub fn warn(&self, msg: String, attachment: Option<(String, Vec<u8>)>) {
        if !self.is_enabled() {
            return;
        }
        let webhook = self.webhook.clone();
        let client = self.client.clone();
        tokio::spawn(async move {
            if let Err(e) = send(&client, &webhook, &msg, attachment).await {
                warn!(error = %e, "failed to send notifier webhook");
            }
        });
    }
}

async fn send(
    client: &Client,
    webhook: &str,
    msg: &str,
    attachment: Option<(String, Vec<u8>)>,
) -> anyhow::Result<()> {
    let req = match attachment {
        Some((filename, bytes)) => {
            let form = Form::new()
                .text("content", msg.to_owned())
                .part("file", Part::bytes(bytes).file_name(filename));
            client.post(webhook).multipart(form)
        }
        None => client.post(webhook).json(&serde_json::json!({"content": msg})),
    };
    let resp = req.send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("webhook returned {}", resp.status());
    }
    Ok(())
}
