//! Commands backing the Collaboration panel and future network transports.
use crate::commands::{CommandSpec, int};
use crate::{EngineError, Result, Session};
use photocraft_doc::LayerId;
use serde_json::{Value, json};

fn bad(cmd: &str, msg: impl Into<String>) -> EngineError {
    EngineError::BadParams { cmd: cmd.into(), msg: msg.into() }
}
fn has_doc(s: &Session) -> std::result::Result<(), String> {
    s.active().map(|_| ()).ok_or_else(|| "no document open".into())
}
fn connected(s: &Session) -> std::result::Result<(), String> {
    s.collab.connected.then_some(()).ok_or_else(|| "start or join a collaboration session first".into())
}
fn user(s: &Session, p: &Value, cmd: &str) -> Result<String> {
    p.get("user")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .or_else(|| s.collab.current_user.clone())
        .ok_or_else(|| bad(cmd, "provide `user` or start a session"))
}
fn layer(p: &Value, cmd: &str) -> Result<Option<LayerId>> {
    p.get("layer").map(|v| v.as_u64().map(LayerId).ok_or_else(|| bad(cmd, "`layer` must be an unsigned integer"))).transpose()
}
fn cursor(p: &Value, cmd: &str) -> Result<Option<[f32; 2]>> {
    let Some(v) = p.get("cursor") else { return Ok(None) };
    let Some(a) = v.as_array() else {
        return Err(bad(cmd, "`cursor` must be [x,y]"));
    };
    if a.len() != 2 {
        return Err(bad(cmd, "`cursor` must contain two numbers"));
    }
    let x = a[0].as_f64().filter(|x| x.is_finite()).ok_or_else(|| bad(cmd, "cursor x is invalid"))? as f32;
    let y = a[1].as_f64().filter(|x| x.is_finite()).ok_or_else(|| bad(cmd, "cursor y is invalid"))? as f32;
    Ok(Some([x, y]))
}
fn start(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "collab.start";
    let document = s.active().ok_or(EngineError::NoDocument)?.doc.id;
    let project = p.get("project").and_then(Value::as_str).filter(|x| !x.trim().is_empty()).unwrap_or("PhotoCraft project").to_string();
    let name = p.get("user").and_then(Value::as_str).unwrap_or("You").trim().to_string();
    s.collab.start(document, project, name).map_err(|e| bad(CMD, e))?;
    Ok(status(s))
}
fn join(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "collab.join";
    let document = s.active().ok_or(EngineError::NoDocument)?.doc.id;
    let name = user(s, p, CMD)?;
    let color = p
        .get("color")
        .and_then(Value::as_array)
        .filter(|a| a.len() == 3)
        .map(|a| [a[0].as_f64().unwrap_or(0.4) as f32, a[1].as_f64().unwrap_or(0.7) as f32, a[2].as_f64().unwrap_or(1.0) as f32])
        .unwrap_or([0.4, 0.8, 0.5]);
    s.collab.join(document, name, color).map_err(|e| bad(CMD, e))?;
    Ok(status(s))
}
fn leave(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "collab.leave";
    let name = user(s, p, CMD)?;
    let found = s.collab.leave(&name);
    if !found {
        return Err(bad(CMD, "collaborator is not connected"));
    }
    Ok(json!({"user": name, "status": "offline"}))
}
fn update_presence(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "collab.presence";
    let name = user(s, p, CMD)?;
    let c = cursor(p, CMD)?;
    let l = layer(p, CMD)?;
    s.collab.presence(&name, c, l, p.get("status").and_then(Value::as_str).map(str::to_string)).map_err(|e| bad(CMD, e))?;
    Ok(json!({"user": name, "cursor": c, "layer": l.map(|x| x.0)}))
}
fn request(s: &mut Session, p: &Value) -> Result<Value> {
    const CMD: &str = "collab.request";
    connected(s).map_err(|e| bad(CMD, e))?;
    let author = user(s, p, CMD)?;
    let summary =
        p.get("summary").and_then(Value::as_str).filter(|x| !x.trim().is_empty()).ok_or_else(|| bad(CMD, "`summary` is required"))?.trim().to_string();
    let command = p.get("command").and_then(Value::as_str).map(str::to_string);
    let id = s.collab.request(author.clone(), command, p.get("params").cloned().unwrap_or(Value::Null), summary, layer(p, CMD)?);
    Ok(json!({"request": id, "author": author, "status": "pending"}))
}
fn review(s: &mut Session, p: &Value, accepted: bool) -> Result<Value> {
    const CMD: &str = "collab.review";
    connected(s).map_err(|e| bad(CMD, e))?;
    let id = int(p, "request").and_then(|x| u64::try_from(x).ok()).ok_or_else(|| bad(CMD, "`request` must be a non-negative integer"))?;
    let reviewer = user(s, p, CMD)?;
    let pending = s.collab.requests.get(&id).cloned().ok_or_else(|| bad(CMD, format!("no change request {id}")))?;
    if accepted && let Some(command) = pending.command.as_deref() {
        s.execute(command, pending.params.clone()).map_err(|e| bad(CMD, format!("the proposed edit failed: {e}")))?;
    }
    let result = s.collab.review(id, accepted, &reviewer).map_err(|e| bad(CMD, e))?;
    Ok(json!({"request": result.id, "status": result.status, "reviewer": reviewer}))
}
fn accept(s: &mut Session, p: &Value) -> Result<Value> {
    review(s, p, true)
}
fn reject(s: &mut Session, p: &Value) -> Result<Value> {
    review(s, p, false)
}
fn list(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(status(s))
}
fn audit(s: &mut Session, _: &Value) -> Result<Value> {
    Ok(json!({"audit": s.collab.audit_json()}))
}
fn status(s: &Session) -> Value {
    json!({"connected": s.collab.connected, "project": s.collab.project, "currentUser": s.collab.current_user, "participants": s.collab.participants_json(), "changes": s.collab.requests_json()})
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: "collab.start",
            label: "Start Collaboration",
            menu: &["Window", "Collaboration"],
            shortcut: None,
            params: r##"{"project":str?,"user":str?} → session status"##,
            enabled: has_doc,
            run: start,
            journal: true,
        },
        CommandSpec {
            id: "collab.join",
            label: "Join Collaboration",
            menu: &["Window", "Collaboration"],
            shortcut: None,
            params: r##"{"user":str,"color":[r,g,b]?} → session status"##,
            enabled: has_doc,
            run: join,
            journal: true,
        },
        CommandSpec {
            id: "collab.leave",
            label: "Leave Collaboration",
            menu: &["Window", "Collaboration"],
            shortcut: None,
            params: r##"{"user":str?} → offline status"##,
            enabled: connected,
            run: leave,
            journal: true,
        },
        CommandSpec {
            id: "collab.presence",
            label: "Update Presence",
            menu: &[],
            shortcut: None,
            params: r##"{"user":str?,"cursor":[x,y]?,”layer”:id?,”status”:str?} → presence"##,
            enabled: connected,
            run: update_presence,
            journal: false,
        },
        CommandSpec {
            id: "collab.request",
            label: "Propose Change",
            menu: &["Window", "Collaboration"],
            shortcut: None,
            params: r##"{"summary":str,"command":str?,"params":object?,"layer":id?,"user":str?} → pending request"##,
            enabled: connected,
            run: request,
            journal: true,
        },
        CommandSpec {
            id: "collab.accept",
            label: "Accept Change",
            menu: &["Window", "Collaboration"],
            shortcut: None,
            params: r##"{"request":id,"user":str?} → accepted request"##,
            enabled: connected,
            run: accept,
            journal: true,
        },
        CommandSpec {
            id: "collab.reject",
            label: "Reject Change",
            menu: &["Window", "Collaboration"],
            shortcut: None,
            params: r##"{"request":id,"user":str?} → rejected request"##,
            enabled: connected,
            run: reject,
            journal: true,
        },
        CommandSpec {
            id: "collab.list",
            label: "Collaborators",
            menu: &[],
            shortcut: None,
            params: "{} → participants and pending changes",
            enabled: has_doc,
            run: list,
            journal: false,
        },
        CommandSpec {
            id: "collab.audit",
            label: "Collaboration Audit",
            menu: &[],
            shortcut: None,
            params: "{} → per-user audit history",
            enabled: has_doc,
            run: audit,
            journal: false,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collaboration_commands_support_reviewable_edits() {
        let mut s = Session::new();
        s.execute("file.new", json!({"width": 8, "height": 8})).expect("new");
        s.execute("collab.start", json!({"project": "team", "user": "Mona"})).expect("start");
        s.execute("collab.join", json!({"user": "Ali"})).expect("join");
        let id = s.execute("collab.request", json!({"user": "Ali", "summary": "Review colors", "layer": 1})).expect("request")["request"].clone();
        s.execute("collab.accept", json!({"request": id, "user": "Mona"})).expect("accept");
        assert_eq!(s.execute("collab.audit", json!({})).expect("audit")["audit"].as_array().map(Vec::len), Some(2));
    }
}
