//! Shared collaboration state: presence, live cursors, reviewable edits and audit history.
//! Transport adapters can mirror these commands over a socket or server without changing the
//! document engine; the UI and CLI use the same state and command surface.
use photocraft_doc::{DocId, LayerId};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug)]
pub struct Collaborator {
    pub id: String,
    pub name: String,
    pub color: [f32; 3],
    pub cursor: Option<[f32; 2]>,
    pub layer: Option<LayerId>,
    pub status: String,
    pub last_seen: u64,
}

#[derive(Clone, Debug)]
pub struct ChangeRequest {
    pub id: u64,
    pub author: String,
    pub command: Option<String>,
    pub params: Value,
    pub summary: String,
    pub layer: Option<LayerId>,
    pub status: String,
    pub created_at: u64,
    pub reviewed_at: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct AuditEntry {
    pub id: u64,
    pub author: String,
    pub action: String,
    pub summary: String,
    pub layer: Option<LayerId>,
    pub timestamp: u64,
}

#[derive(Clone, Debug, Default)]
pub struct CollaborationState {
    pub document: Option<DocId>,
    pub project: Option<String>,
    pub current_user: Option<String>,
    pub connected: bool,
    pub participants: BTreeMap<String, Collaborator>,
    pub requests: BTreeMap<u64, ChangeRequest>,
    pub audit: Vec<AuditEntry>,
    next_id: u64,
}

impl CollaborationState {
    pub fn start(&mut self, document: DocId, project: String, user: String) -> Result<(), String> {
        if user.trim().is_empty() {
            return Err("user name cannot be empty".into());
        }
        self.document = Some(document);
        self.project = Some(project);
        self.current_user = Some(user.clone());
        self.connected = true;
        self.participants.entry(user.clone()).or_insert_with(|| Collaborator {
            id: user.clone(),
            name: user,
            color: [0.25, 0.65, 1.0],
            cursor: None,
            layer: None,
            status: "editing".into(),
            last_seen: now(),
        });
        Ok(())
    }

    pub fn join(&mut self, document: DocId, user: String, color: [f32; 3]) -> Result<(), String> {
        if user.trim().is_empty() {
            return Err("user name cannot be empty".into());
        }
        if self.document.is_none() {
            self.document = Some(document);
        }
        self.connected = true;
        self.current_user.get_or_insert_with(|| user.clone());
        self.participants.insert(
            user.clone(),
            Collaborator { id: user.clone(), name: user, color, cursor: None, layer: None, status: "reviewing".into(), last_seen: now() },
        );
        Ok(())
    }

    pub fn leave(&mut self, user: &str) -> bool {
        if let Some(person) = self.participants.get_mut(user) {
            person.status = "offline".into();
            person.last_seen = now();
            true
        } else {
            false
        }
    }

    pub fn presence(&mut self, user: &str, cursor: Option<[f32; 2]>, layer: Option<LayerId>, status: Option<String>) -> Result<(), String> {
        let person = self.participants.get_mut(user).ok_or_else(|| format!("collaborator `{user}` is not connected"))?;
        person.cursor = cursor;
        person.layer = layer;
        if let Some(status) = status {
            person.status = status;
        }
        person.last_seen = now();
        Ok(())
    }

    pub fn request(&mut self, author: String, command: Option<String>, params: Value, summary: String, layer: Option<LayerId>) -> u64 {
        let id = self.next();
        self.requests.insert(
            id,
            ChangeRequest {
                id,
                author: author.clone(),
                command,
                params,
                summary: summary.clone(),
                layer,
                status: "pending".into(),
                created_at: now(),
                reviewed_at: None,
            },
        );
        self.audit.push(AuditEntry { id, author, action: "proposed".into(), summary, layer, timestamp: now() });
        id
    }

    pub fn review(&mut self, id: u64, accepted: bool, reviewer: &str) -> Result<ChangeRequest, String> {
        let request = self.requests.get_mut(&id).ok_or_else(|| format!("no change request {id}"))?;
        if request.status != "pending" {
            return Err(format!("change request {id} is already {}", request.status));
        }
        request.status = if accepted { "accepted" } else { "rejected" }.into();
        request.reviewed_at = Some(now());
        self.audit.push(AuditEntry {
            id,
            author: reviewer.into(),
            action: request.status.clone(),
            summary: request.summary.clone(),
            layer: request.layer,
            timestamp: now(),
        });
        Ok(request.clone())
    }

    fn next(&mut self) -> u64 {
        self.next_id = self.next_id.saturating_add(1);
        self.next_id
    }
    pub fn participants_json(&self) -> Vec<Value> {
        self.participants.values().map(|p| json!({"id": p.id, "name": p.name, "color": p.color, "cursor": p.cursor, "layer": p.layer.map(|x| x.0), "status": p.status, "lastSeen": p.last_seen})).collect()
    }
    pub fn requests_json(&self) -> Vec<Value> {
        self.requests.values().map(|r| json!({"id": r.id, "author": r.author, "command": r.command, "params": r.params, "summary": r.summary, "layer": r.layer.map(|x| x.0), "status": r.status, "createdAt": r.created_at, "reviewedAt": r.reviewed_at})).collect()
    }
    pub fn audit_json(&self) -> Vec<Value> {
        self.audit
            .iter()
            .map(|a| json!({"id": a.id, "author": a.author, "action": a.action, "summary": a.summary, "layer": a.layer.map(|x| x.0), "timestamp": a.timestamp}))
            .collect()
    }

    /// Mirror a server-assigned request id without creating a second local id.
    pub fn remote_request(&mut self, id: u64, author: String, command: Option<String>, params: Value, summary: String, layer: Option<LayerId>) {
        self.next_id = self.next_id.max(id);
        self.requests.entry(id).or_insert(ChangeRequest {
            id,
            author,
            command,
            params,
            summary,
            layer,
            status: "pending".into(),
            created_at: now(),
            reviewed_at: None,
        });
    }

    pub fn remote_review(&mut self, id: u64, status: &str, reviewer: &str) -> Result<(), String> {
        let request = self.requests.get_mut(&id).ok_or_else(|| format!("no change request {id}"))?;
        request.status = status.to_string();
        request.reviewed_at = Some(now());
        self.audit.push(AuditEntry {
            id,
            author: reviewer.to_string(),
            action: status.to_string(),
            summary: request.summary.clone(),
            layer: request.layer,
            timestamp: now(),
        });
        Ok(())
    }
}
fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn presence_and_review_are_recorded_per_user() {
        let mut c = CollaborationState::default();
        let doc = DocId(7);
        c.start(doc, "team-file".into(), "Mona".into()).expect("start");
        c.join(doc, "Ali".into(), [1.0, 0.4, 0.2]).expect("join");
        c.presence("Ali", Some([12.0, 8.0]), Some(LayerId(3)), Some("editing".into())).expect("presence");
        let id = c.request("Ali".into(), None, Value::Null, "Adjust layer colors".into(), Some(LayerId(3)));
        c.review(id, true, "Mona").expect("accept");
        assert_eq!(c.participants["Ali"].cursor, Some([12.0, 8.0]));
        assert_eq!(c.requests[&id].status, "accepted");
        assert_eq!(c.audit.len(), 3);
    }
}
