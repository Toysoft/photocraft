//! Authenticated WebSocket transport for PhotoCraft Live Collaboration.
//!
//! The engine owns document semantics; this module owns transport and room membership.
//! Clients authenticate with `{type:"auth", token, project, user}` and then exchange:
//! `presence`, `propose`, `review`, and `command` messages. Commands carry `baseRevision`;
//! the room rejects stale commands instead of silently applying edits to an old document.
//!
//! This is deliberately a relay with an authoritative room revision. A desktop or headless
//! client applies an accepted command through its normal engine command registry and reports the
//! resulting revision. No pixel buffers cross the wire.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio_tungstenite::{accept_async, tungstenite::Message};

const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug)]
pub struct CollaborationServerConfig {
    pub bind: String,
    pub token: String,
    pub max_clients_per_project: usize,
}

impl Default for CollaborationServerConfig {
    fn default() -> Self {
        Self { bind: "127.0.0.1:7879".into(), token: String::new(), max_clients_per_project: 32 }
    }
}

#[derive(Debug, Error)]
pub enum CollaborationServerError {
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("WebSocket: {0}")]
    WebSocket(#[from] tokio_tungstenite::tungstenite::Error),
}

type Out = mpsc::UnboundedSender<Message>;

type Shared = Arc<Mutex<Hub>>;

struct Client {
    user: String,
    out: Out,
}

struct Room {
    revision: u64,
    clients: BTreeMap<u64, Client>,
    log: Vec<Value>,
}

#[derive(Default)]
struct Hub {
    rooms: BTreeMap<String, Room>,
    next_client: u64,
}

/// A WebSocket collaboration server. Clone the server only when embedding it in a host that
/// needs to inspect the hub; `serve` is the long-running accept loop.
#[derive(Clone)]
pub struct CollaborationServer {
    config: CollaborationServerConfig,
    hub: Shared,
}

impl CollaborationServer {
    pub fn new(config: CollaborationServerConfig) -> Self {
        Self { config, hub: Arc::new(Mutex::new(Hub::default())) }
    }

    pub fn config(&self) -> &CollaborationServerConfig {
        &self.config
    }

    /// Bind and serve until the listener fails. The bind address is intentionally configurable;
    /// deployments exposed to the Internet must put TLS/authentication in front of this server or
    /// use a TLS-capable reverse proxy.
    pub async fn serve(&self) -> Result<(), CollaborationServerError> {
        let listener = TcpListener::bind(&self.config.bind).await?;
        loop {
            let (stream, peer) = listener.accept().await?;
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(error) = server.connection(stream).await {
                    eprintln!("photocraft collaboration connection {peer}: {error}");
                }
            });
        }
    }

    async fn connection(&self, stream: TcpStream) -> Result<(), CollaborationServerError> {
        let socket = accept_async(stream).await?;
        let (mut sink, mut source) = socket.split();
        let auth = tokio::time::timeout(AUTH_TIMEOUT, source.next()).await;
        let first = match auth {
            Ok(Some(Ok(Message::Text(text)))) => text,
            Ok(Some(Ok(_))) => return Self::send_error(&mut sink, "first message must be auth").await,
            Ok(Some(Err(error))) => return Err(error.into()),
            Ok(None) => return Ok(()),
            Err(_) => return Self::send_error(&mut sink, "authentication timeout").await,
        };
        let auth: Value = match serde_json::from_str(&first) {
            Ok(value) => value,
            Err(_) => return Self::send_error(&mut sink, "invalid authentication JSON").await,
        };
        if auth.get("type").and_then(Value::as_str) != Some("auth") || auth.get("token").and_then(Value::as_str) != Some(self.config.token.as_str()) {
            return Self::send_error(&mut sink, "authentication failed").await;
        }
        let project = match non_empty(&auth, "project") {
            Some(value) => value.to_owned(),
            None => return Self::send_error(&mut sink, "auth requires project").await,
        };
        let user = match non_empty(&auth, "user") {
            Some(value) => value.to_owned(),
            None => return Self::send_error(&mut sink, "auth requires user").await,
        };
        let (out, mut inbox) = mpsc::unbounded_channel::<Message>();
        let writer = tokio::spawn(async move {
            while let Some(message) = inbox.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
        });
        let client_id = self.join(&project, user.clone(), out.clone()).await?;
        let _ = out.send(Message::Text(json!({"type":"auth.ok", "clientId": client_id}).to_string().into()));
        let result = self.read_loop(&mut source, &project, &user, client_id, out).await;
        self.leave(&project, client_id).await;
        writer.abort();
        result
    }

    async fn read_loop<S>(&self, source: &mut S, project: &str, user: &str, client_id: u64, out: Out) -> Result<(), CollaborationServerError>
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        while let Some(message) = source.next().await {
            let message = message?;
            let Message::Text(text) = message else {
                if message.is_close() {
                    break;
                }
                continue;
            };
            if text.len() > MAX_MESSAGE_BYTES {
                let _ = out.send(Message::Text(error_json("message too large").into()));
                continue;
            }
            let value: Value = match serde_json::from_str(&text) {
                Ok(value) => value,
                Err(_) => {
                    let _ = out.send(Message::Text(error_json("invalid JSON").into()));
                    continue;
                }
            };
            self.handle(project, user, client_id, value).await;
        }
        Ok(())
    }

    async fn join(&self, project: &str, user: String, out: Out) -> Result<u64, CollaborationServerError> {
        let mut hub = self.hub.lock().await;
        let id = {
            hub.next_client = hub.next_client.saturating_add(1);
            hub.next_client
        };
        let room = hub.rooms.entry(project.to_owned()).or_insert_with(|| Room { revision: 0, clients: BTreeMap::new(), log: Vec::new() });
        if room.clients.len() >= self.config.max_clients_per_project {
            return Err(CollaborationServerError::Io(std::io::Error::other("project client limit reached")));
        }
        let snapshot = json!({"type":"room.snapshot", "project": project, "revision": room.revision, "participants": room.clients.values().map(|c| json!({"user": c.user})).collect::<Vec<_>>(), "log": room.log});
        room.clients.insert(id, Client { user: user.clone(), out: out.clone() });
        let _ = out.send(Message::Text(snapshot.to_string().into()));
        broadcast(room, id, json!({"type":"presence.join", "clientId": id, "user": user}));
        Ok(id)
    }

    async fn leave(&self, project: &str, client_id: u64) {
        let mut hub = self.hub.lock().await;
        let mut remove_room = false;
        if let Some(room) = hub.rooms.get_mut(project) {
            let user = room.clients.remove(&client_id).map(|c| c.user);
            if let Some(user) = user {
                broadcast(room, client_id, json!({"type":"presence.leave", "clientId": client_id, "user": user}));
            }
            remove_room = room.clients.is_empty();
        }
        if remove_room {
            hub.rooms.remove(project);
        }
    }

    async fn handle(&self, project: &str, user: &str, client_id: u64, value: Value) {
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let mut hub = self.hub.lock().await;
        let Some(room) = hub.rooms.get_mut(project) else { return };
        match kind {
            "presence" => broadcast(
                room,
                client_id,
                json!({"type":"presence", "clientId": client_id, "user": user, "cursor": value.get("cursor"), "layer": value.get("layer"), "status": value.get("status")}),
            ),
            "propose" => {
                let id = room.revision.saturating_add(room.log.len() as u64 + 1);
                let proposal = json!({"type":"change.proposed", "request": id, "author": user, "command": value.get("command"), "params": value.get("params").cloned().unwrap_or(Value::Null), "summary": value.get("summary"), "baseRevision": value.get("baseRevision").and_then(Value::as_u64).unwrap_or(room.revision), "status":"pending"});
                room.log.push(proposal.clone());
                broadcast_all(room, proposal);
            }
            "review" => {
                let accepted = value.get("accepted").and_then(Value::as_bool).unwrap_or(false);
                let event =
                    json!({"type":"change.reviewed", "request": value.get("request"), "reviewer": user, "accepted": accepted, "revision": room.revision});
                broadcast_all(room, event);
            }
            "command" => {
                let base = value.get("baseRevision").and_then(Value::as_u64).unwrap_or(u64::MAX);
                if base != room.revision {
                    if let Some(client) = room.clients.get(&client_id) {
                        let _ = client
                            .out
                            .send(Message::Text(json!({"type":"conflict", "expectedRevision": room.revision, "receivedRevision": base}).to_string().into()));
                    }
                } else {
                    room.revision = room.revision.saturating_add(1);
                    let event = json!({"type":"command", "command": value.get("command"), "params": value.get("params").cloned().unwrap_or(Value::Null), "author": user, "revision": room.revision});
                    room.log.push(event.clone());
                    broadcast_all(room, event);
                }
            }
            "ping" => {
                if let Some(client) = room.clients.get(&client_id) {
                    let _ = client.out.send(Message::Text(json!({"type":"pong", "revision": room.revision}).to_string().into()));
                }
            }
            _ => {
                if let Some(client) = room.clients.get(&client_id) {
                    let _ = client.out.send(Message::Text(error_json("unknown collaboration message").into()));
                }
            }
        }
    }

    async fn send_error<S>(sink: &mut S, message: &str) -> Result<(), CollaborationServerError>
    where
        S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
    {
        sink.send(Message::Text(error_json(message).into())).await?;
        Ok(())
    }
}

fn non_empty<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}
fn error_json(message: &str) -> String {
    json!({"type":"error", "error": message}).to_string()
}
fn broadcast(room: &Room, except: u64, value: Value) {
    let message = Message::Text(value.to_string().into());
    for (id, client) in &room.clients {
        if *id != except {
            let _ = client.out.send(message.clone());
        }
    }
}
fn broadcast_all(room: &Room, value: Value) {
    let message = Message::Text(value.to_string().into());
    for client in room.clients.values() {
        let _ = client.out.send(message.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn auth_requires_project_and_user() {
        assert_eq!(non_empty(&json!({"project":" p "}), "project"), Some("p"));
        assert_eq!(non_empty(&json!({"user":""}), "user"), None);
    }
}
