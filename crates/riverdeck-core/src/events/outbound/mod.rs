pub mod applications;
pub mod deep_link;
pub mod devices;
pub mod encoder;
pub mod keypad;
pub mod property_inspector;
pub mod settings;
pub mod states;
pub mod will_appear;

use futures::SinkExt;
use serde::Serialize;
use tokio_tungstenite::tungstenite::Message;

const MAX_QUEUED_MESSAGES_PER_PLUGIN: usize = 256;
const MAX_QUEUED_BYTES_PER_PLUGIN: usize = 2 * 1024 * 1024;
const MAX_QUEUED_MESSAGES_PER_PROPERTY_INSPECTOR: usize = 256;
const MAX_QUEUED_BYTES_PER_PROPERTY_INSPECTOR: usize = 2 * 1024 * 1024;

fn message_size_bytes(m: &Message) -> usize {
    match m {
        Message::Text(s) => s.len(),
        Message::Binary(b) => b.len(),
        // Control frames are tiny and shouldn't be queued often anyway.
        _ => 0,
    }
}

fn retain_newest_within_limits(queue: &mut Vec<Message>, max_msgs: usize, max_bytes: usize) {
    if queue.is_empty() {
        return;
    }

    // If the newest message alone violates the cap, drop it.
    if message_size_bytes(queue.last().unwrap()) > max_bytes {
        let _ = queue.pop();
        return;
    }

    let mut bytes = 0usize;
    let mut start = queue.len();

    // Walk from newest to oldest; keep the largest suffix within caps.
    for (count, idx) in (0..queue.len()).rev().enumerate() {
        let sz = message_size_bytes(&queue[idx]);
        // `count` is a 0-based counter from `enumerate()`.
        let next_count = count + 1;
        if count > 0 && (next_count > max_msgs || bytes + sz > max_bytes) {
            break;
        }
        bytes += sz;
        start = idx;
    }

    if start > 0 {
        queue.drain(0..start);
    }
}

#[derive(Serialize)]
struct Coordinates {
    row: u8,
    column: u8,
}

#[derive(Serialize)]
#[allow(non_snake_case)]
struct GenericInstancePayload {
    settings: serde_json::Value,
    coordinates: Coordinates,
    controller: String,
    state: u16,
    isInMultiAction: bool,
}

impl GenericInstancePayload {
    fn new(instance: &crate::shared::ActionInstance) -> Self {
        let coordinates = match &instance.context.controller[..] {
            "Encoder" => Coordinates {
                row: 0,
                column: instance.context.position,
            },
            _ => {
                let columns = crate::shared::DEVICES
                    .get(&instance.context.device)
                    .unwrap()
                    .columns;
                Coordinates {
                    row: instance.context.position / columns,
                    column: instance.context.position % columns,
                }
            }
        };

        Self {
            settings: instance.settings.clone(),
            coordinates,
            controller: instance.context.controller.clone(),
            state: instance.current_state,
            isInMultiAction: instance.context.index != 0,
        }
    }
}

async fn send_to_plugin(plugin: &str, data: &impl Serialize) -> Result<(), anyhow::Error> {
    let message = Message::Text(serde_json::to_string(data)?.into());
    let mut sockets = super::PLUGIN_SOCKETS.lock().await;

    if let Some(socket) = sockets.get_mut(plugin) {
        socket.send(message).await?;
    } else {
        let mut queues = super::PLUGIN_QUEUES.write().await;
        let q = queues.entry(plugin.to_owned()).or_default();
        q.push(message);
        retain_newest_within_limits(
            q,
            MAX_QUEUED_MESSAGES_PER_PLUGIN,
            MAX_QUEUED_BYTES_PER_PLUGIN,
        );
    }

    Ok(())
}

async fn send_to_all_plugins(data: &impl Serialize) -> Result<(), anyhow::Error> {
    let plugins_dir = crate::shared::config_dir().join("plugins");
    let plugins_canon = tokio::fs::canonicalize(&plugins_dir)
        .await
        .unwrap_or(plugins_dir.clone());
    let mut entries = tokio::fs::read_dir(&plugins_dir).await?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let entry_path = entry.path();
        let meta = tokio::fs::symlink_metadata(&entry_path).await?;
        let path = if meta.file_type().is_symlink() {
            let target = tokio::fs::read_link(&entry_path).await?;
            let target_canon = tokio::fs::canonicalize(&target)
                .await
                .unwrap_or(target.clone());
            if !target_canon.starts_with(&plugins_canon) {
                continue;
            }
            target
        } else {
            entry_path
        };
        let metadata = tokio::fs::metadata(&path).await?;
        if metadata.is_dir() {
            let _ = send_to_plugin(entry.file_name().to_str().unwrap(), data).await;
        }
    }
    Ok(())
}

#[allow(clippy::map_entry)]
async fn send_to_property_inspector(
    context: &crate::shared::ActionContext,
    data: &impl Serialize,
) -> Result<(), anyhow::Error> {
    let message = Message::Text(serde_json::to_string(data)?.into());
    let mut sockets = super::PROPERTY_INSPECTOR_SOCKETS.lock().await;

    if let Some(socket) = sockets.get_mut(&context.to_string()) {
        socket.send(message).await?;
    } else {
        let mut queues = super::PROPERTY_INSPECTOR_QUEUES.write().await;
        let key = context.to_string();
        let q = queues.entry(key).or_default();
        q.push(message);
        retain_newest_within_limits(
            q,
            MAX_QUEUED_MESSAGES_PER_PROPERTY_INSPECTOR,
            MAX_QUEUED_BYTES_PER_PROPERTY_INSPECTOR,
        );
    }

    Ok(())
}
