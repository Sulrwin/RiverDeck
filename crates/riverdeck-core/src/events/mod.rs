pub mod inbound;
pub mod outbound;

use inbound::RegisterEvent;

use std::collections::HashMap;
use std::collections::HashSet;

use futures::{SinkExt, StreamExt, stream::SplitSink};
use once_cell::sync::Lazy;
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

type Sockets = Lazy<Mutex<HashMap<String, SplitSink<WebSocketStream<TcpStream>, Message>>>>;
static PLUGIN_SOCKETS: Sockets = Lazy::new(|| Mutex::new(HashMap::new()));
static PROPERTY_INSPECTOR_SOCKETS: Sockets = Lazy::new(|| Mutex::new(HashMap::new()));
static PLUGIN_QUEUES: Lazy<RwLock<HashMap<String, Vec<Message>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));
static PROPERTY_INSPECTOR_QUEUES: Lazy<RwLock<HashMap<String, Vec<Message>>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

static PROPERTY_INSPECTOR_AUTHED: Lazy<Mutex<HashSet<String>>> =
    Lazy::new(|| Mutex::new(HashSet::new()));

fn allow_unauthenticated_property_inspectors() -> bool {
    // Escape hatch for debugging / compatibility.
    std::env::var("RIVERDECK_PI_ALLOW_UNAUTH")
        .ok()
        .as_deref()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

pub(crate) async fn set_property_inspector_authed(uuid: &str, authed: bool) {
    let mut set = PROPERTY_INSPECTOR_AUTHED.lock().await;
    if authed {
        set.insert(uuid.to_owned());
    } else {
        set.remove(uuid);
    }
}

pub(crate) async fn is_property_inspector_authed(uuid: &str) -> bool {
    if allow_unauthenticated_property_inspectors() {
        return true;
    }
    PROPERTY_INSPECTOR_AUTHED.lock().await.contains(uuid)
}

pub async fn registered_plugins() -> Vec<String> {
    PLUGIN_SOCKETS
        .lock()
        .await
        .keys()
        .map(|x| x.to_owned())
        .collect()
}

/// Register a plugin or property inspector to send and receive events with its WebSocket.
pub async fn register_plugin(event: RegisterEvent, stream: WebSocketStream<TcpStream>) {
    let (mut read, write) = stream.split();
    match event {
        RegisterEvent::RegisterPlugin { uuid } => {
            log::debug!("Registered plugin {}", uuid);
            // Drain queued messages (if any) to avoid unbounded memory growth.
            let queued = { PLUGIN_QUEUES.write().await.remove(&uuid) };
            if let Some(queue) = queued {
                for message in queue {
                    let _ = read.feed(message).await;
                }
                let _ = read.flush().await;
            }
            PLUGIN_SOCKETS.lock().await.insert(uuid.clone(), read);
            tokio::spawn(async move {
                let uuid = uuid;
                write
                    .for_each(|event| inbound::process_incoming_message(event, &uuid, false))
                    .await;
                PLUGIN_SOCKETS.lock().await.remove(&uuid);
            });
        }
        RegisterEvent::RegisterPropertyInspector { uuid } => {
            // Mark as unauthenticated until we see a `riverdeckAuth` message on this socket.
            set_property_inspector_authed(&uuid, false).await;
            // Drain queued messages (if any) to avoid unbounded memory growth.
            let queued = { PROPERTY_INSPECTOR_QUEUES.write().await.remove(&uuid) };
            if let Some(queue) = queued {
                for message in queue {
                    let _ = read.feed(message).await;
                }
                let _ = read.flush().await;
            }
            PROPERTY_INSPECTOR_SOCKETS
                .lock()
                .await
                .insert(uuid.clone(), read);
            tokio::spawn(async move {
                let uuid = uuid;
                write
                    .for_each(|event| inbound::process_incoming_message_pi(event, &uuid))
                    .await;
                PROPERTY_INSPECTOR_SOCKETS.lock().await.remove(&uuid);
                set_property_inspector_authed(&uuid, false).await;
            });
        }
    };
}
