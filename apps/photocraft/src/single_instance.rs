//! One PhotoCraft per user on Windows and Linux (#3708): a file opened from Explorer or a file
//! manager lands as a new tab in the running window, like Photoshop, instead of starting a second
//! copy of the program.
//!
//! The first launch takes an exclusive OS lock on `instance.lock` in the settings directory (the OS
//! drops it when the process ends, crash included), listens on a loopback port and writes the port
//! plus a fresh 256-bit token to `instance.json`. A later launch that finds the lock taken sends its
//! paths there and exits; the primary queues them as [`OsEvent::Open`], which opens each one and
//! brings the window forward. Explorer starts one process per file when several are opened at once;
//! the lock decides the race, so they all end up as tabs in one window.
//!
//! Anything that goes wrong on the way (no settings directory, a hung or vanished primary, a stale
//! `instance.json`) starts a normal instance instead: the user's file always opens somewhere.
//! macOS doesn't need this: LaunchServices already sends a running app an open-documents event.

// Only the tests use it on macOS.
#![cfg_attr(target_os = "macos", allow(dead_code))]

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use photocraft_automation::security::{LineRead, generate_token, read_bounded_line, token_matches};
use photocraft_ui_egui::{OsEvent, OsEventsFn};
use serde_json::{Value, json};

const LOCK_FILE: &str = "instance.lock";
const INFO_FILE: &str = "instance.json";
/// How long a second launch keeps trying to reach a primary that is still starting up.
const HANDOFF_WAIT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// More paths than this in one request is not a file manager.
const MAX_PATHS: usize = 4096;
/// Launches served at once; a stalled client holds only its own slot, never the next launch.
const MAX_CLIENTS: usize = 8;

/// Outcome of [`claim`].
pub enum Launch {
    /// This process is the running instance; keep [`Primary`] alive and attach it to the app.
    Primary(Primary),
    /// The running instance took the paths; this process should exit.
    Forwarded,
    /// Couldn't do either (reason given): run as an independent instance.
    Standalone(String),
}

/// The running instance: holds the lock and receives other launches' paths.
pub struct Primary {
    _lock: File,
    inbox: Arc<Mutex<Inbox>>,
}

#[derive(Default)]
struct Inbox {
    events: Vec<OsEvent>,
    ctx: Option<egui::Context>,
}

impl Primary {
    /// The queue the UI drains (see `Services::os_events`); a later launch wakes `ctx`.
    pub fn connect(&self, ctx: &egui::Context) -> OsEventsFn {
        let mut inbox = self.inbox.lock().unwrap_or_else(PoisonError::into_inner);
        if !inbox.events.is_empty() {
            ctx.request_repaint();
        }
        inbox.ctx = Some(ctx.clone());
        drop(inbox);
        let inbox = Arc::clone(&self.inbox);
        Box::new(move || std::mem::take(&mut inbox.lock().unwrap_or_else(PoisonError::into_inner).events))
    }
}

/// Become the running instance under `dir` or hand `paths` (absolute) to the one that is.
pub fn claim(dir: &Path, paths: &[String]) -> Launch {
    match try_claim(dir, paths) {
        Ok(launch) => launch,
        Err(e) => Launch::Standalone(e),
    }
}

fn try_claim(dir: &Path, paths: &[String]) -> Result<Launch, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let lock_path = dir.join(LOCK_FILE);
    let lock = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&lock_path).map_err(|e| format!("{}: {e}", lock_path.display()))?;
    match lock.try_lock() {
        Ok(()) => serve(dir, lock).map(Launch::Primary),
        Err(std::fs::TryLockError::WouldBlock) => forward(dir, paths).map(|()| Launch::Forwarded),
        Err(std::fs::TryLockError::Error(e)) => Err(format!("{}: {e}", lock_path.display())),
    }
}

fn serve(dir: &Path, lock: File) -> Result<Primary, String> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|e| format!("loopback listener: {e}"))?;
    let port = listener.local_addr().map_err(|e| format!("loopback listener: {e}"))?.port();
    let token = generate_token().map_err(|e| e.to_string())?;
    write_info(dir, port, &token)?;
    let inbox: Arc<Mutex<Inbox>> = Arc::default();
    let queue = Arc::clone(&inbox);
    let spawned = std::thread::Builder::new().name("single-instance".into()).spawn(move || {
        let token = Arc::new(token);
        let clients = Arc::new(AtomicUsize::new(0));
        for stream in listener.incoming().flatten() {
            // Each launch on its own thread, so one that connects and stalls doesn't hold the
            // ones after it; past the cap a connection is dropped and that launch retries.
            if clients.fetch_add(1, Ordering::AcqRel) >= MAX_CLIENTS {
                clients.fetch_sub(1, Ordering::AcqRel);
                continue;
            }
            let (token, queue, slot) = (Arc::clone(&token), Arc::clone(&queue), Arc::clone(&clients));
            let spawned = std::thread::Builder::new().name("single-instance-client".into()).spawn(move || {
                if let Some(paths) = receive(stream, &token) {
                    let mut inbox = queue.lock().unwrap_or_else(PoisonError::into_inner);
                    inbox.events.push(OsEvent::Open(paths));
                    if let Some(ctx) = &inbox.ctx {
                        ctx.request_repaint();
                    }
                }
                slot.fetch_sub(1, Ordering::AcqRel);
            });
            if spawned.is_err() {
                clients.fetch_sub(1, Ordering::AcqRel);
            }
        }
    });
    spawned.map_err(|e| format!("single-instance listener thread: {e}"))?;
    Ok(Primary { _lock: lock, inbox })
}

/// Port and token, written to a temporary file and renamed so a reader never sees half of it.
/// On Unix the file is the user's alone (0600): another local user who read the token could make
/// this app open any path. (Windows keeps the per-user ACL of the settings folder.)
fn write_info(dir: &Path, port: u16, token: &str) -> Result<(), String> {
    let path = dir.join(INFO_FILE);
    let tmp = dir.join(format!("{INFO_FILE}.{}.tmp", std::process::id()));
    let body = json!({"port": port, "token": token, "pid": std::process::id()}).to_string();
    // A leftover from a crash could carry wider permissions; the mode applies only on create.
    let _ = std::fs::remove_file(&tmp);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = options.open(&tmp).and_then(|mut f| f.write_all(body.as_bytes())).and_then(|()| std::fs::rename(&tmp, &path));
    written.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

fn read_info(dir: &Path) -> Option<(u16, String)> {
    let v: Value = serde_json::from_slice(&std::fs::read(dir.join(INFO_FILE)).ok()?).ok()?;
    let port = u16::try_from(v.get("port")?.as_u64()?).ok()?;
    Some((port, v.get("token")?.as_str()?.to_string()))
}

/// Read one `{"token":…,"open":[…]}` line; reply and return the paths when the token matches.
fn receive(stream: TcpStream, token: &str) -> Option<Vec<String>> {
    stream.set_read_timeout(Some(IO_TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).ok()?;
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut line = String::new();
    if !matches!(read_bounded_line(&mut reader, &mut line), Ok(LineRead::Line)) {
        return None;
    }
    let paths = parse_request(&line, token);
    let reply = match &paths {
        Some(_) => json!({"ok": true}),
        None => json!({"ok": false, "error": "rejected"}),
    };
    let mut out = stream;
    let _ = writeln!(out, "{reply}").and_then(|()| out.flush());
    paths
}

fn parse_request(line: &str, token: &str) -> Option<Vec<String>> {
    let v: Value = serde_json::from_str(line).ok()?;
    if !token_matches(token, v.get("token")?.as_str()?) {
        return None;
    }
    let open = v.get("open")?.as_array()?;
    if open.len() > MAX_PATHS {
        return None;
    }
    open.iter().map(|p| p.as_str().map(str::to_string)).collect()
}

/// Send `paths` to the running instance, retrying while it starts up (it may hold the lock before
/// `instance.json` is written, or the file may still name a previous instance's port).
fn forward(dir: &Path, paths: &[String]) -> Result<(), String> {
    let deadline = Instant::now() + HANDOFF_WAIT;
    let mut last = "no instance.json".to_string();
    loop {
        if let Some((port, token)) = read_info(dir) {
            match send(port, &json!({"token": token, "open": paths}).to_string()) {
                Ok(()) => return Ok(()),
                Err(e) => last = e,
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("the running PhotoCraft didn't answer ({last})"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn send(port: u16, request: &str) -> Result<(), String> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, IO_TIMEOUT).map_err(|e| e.to_string())?;
    stream.set_read_timeout(Some(IO_TIMEOUT)).map_err(|e| e.to_string())?;
    stream.set_write_timeout(Some(IO_TIMEOUT)).map_err(|e| e.to_string())?;
    writeln!(stream, "{request}").and_then(|()| stream.flush()).map_err(|e| e.to_string())?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    match read_bounded_line(&mut reader, &mut line) {
        Ok(LineRead::Line) => {}
        Ok(_) => return Err("no reply".into()),
        Err(e) => return Err(e.to_string()),
    }
    let ok = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("ok")?.as_bool()) == Some(true);
    if ok { Ok(()) } else { Err("rejected".into()) }
}

/// Absolute form of a command-line path, so the running instance (another working directory)
/// opens the same file.
pub fn absolute(path: &str) -> String {
    std::path::absolute(path).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("photocraft-single-instance-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn drain(primary: &Primary) -> Vec<OsEvent> {
        let mut f = primary.connect(&egui::Context::default());
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let events = f();
            if !events.is_empty() || Instant::now() >= deadline {
                return events;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn second_launch_hands_its_paths_to_the_first() {
        let dir = temp_dir("handoff");
        let Launch::Primary(primary) = claim(&dir, &[]) else { panic!("first launch should be primary") };
        let paths = vec!["C:\\art\\one.psd".to_string(), "/home/a/two é.png".to_string()];
        assert!(matches!(claim(&dir, &paths), Launch::Forwarded));
        assert_eq!(drain(&primary), vec![OsEvent::Open(paths)]);
        // A plain second start (no files) just brings the window forward.
        assert!(matches!(claim(&dir, &[]), Launch::Forwarded));
        assert_eq!(drain(&primary), vec![OsEvent::Open(Vec::new())]);
        drop(primary);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn simultaneous_launches_all_land_in_one_instance() {
        let dir = temp_dir("race");
        let launches: Vec<_> = (0..6)
            .map(|i| {
                let dir = dir.clone();
                std::thread::spawn(move || claim(&dir, &[format!("file{i}.psd")]))
            })
            .collect();
        let results: Vec<Launch> = launches.into_iter().map(|t| t.join().unwrap()).collect();
        let primaries: Vec<&Primary> = results.iter().filter_map(|l| if let Launch::Primary(p) = l { Some(p) } else { None }).collect();
        assert_eq!(primaries.len(), 1, "exactly one primary");
        let forwarded = results.iter().filter(|l| matches!(l, Launch::Forwarded)).count();
        assert_eq!(forwarded, 5);
        let mut f = primaries[0].connect(&egui::Context::default());
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        while got.len() < 5 && Instant::now() < deadline {
            got.extend(f());
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(got.len(), 5);
        drop(results);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crashed_instance_leaves_nothing_in_the_way() {
        let dir = temp_dir("stale");
        std::fs::create_dir_all(&dir).unwrap();
        // instance.json from a process that is gone; the lock is free.
        std::fs::write(dir.join(INFO_FILE), r#"{"port":1,"token":"x","pid":1}"#).unwrap();
        let Launch::Primary(primary) = claim(&dir, &[]) else { panic!("a free lock makes this launch primary") };
        assert!(read_info(&dir).is_some_and(|(port, _)| port != 1));
        drop(primary);
        // Released with the handle, as on process exit.
        assert!(matches!(claim(&dir, &[]), Launch::Primary(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn requests_without_the_token_or_malformed_are_rejected() {
        let token = generate_token().unwrap();
        assert_eq!(parse_request(&json!({"token": token, "open": ["a"]}).to_string(), &token), Some(vec!["a".to_string()]));
        let wrong = generate_token().unwrap();
        assert_eq!(parse_request(&json!({"token": wrong, "open": ["a"]}).to_string(), &token), None);
        for bad in ["", "not json", "{}", r#"{"token":5}"#, &json!({"token": token}).to_string(), &json!({"token": token, "open": [1]}).to_string()] {
            assert_eq!(parse_request(bad, &token), None, "{bad:?}");
        }
        let many: Vec<String> = (0..=MAX_PATHS).map(|i| i.to_string()).collect();
        assert_eq!(parse_request(&json!({"token": token, "open": many}).to_string(), &token), None);
    }

    #[test]
    fn a_stalled_client_does_not_hold_up_the_next_launch() {
        let dir = temp_dir("stalled");
        let Launch::Primary(primary) = claim(&dir, &[]) else { panic!("first launch should be primary") };
        let (port, _) = read_info(&dir).unwrap();
        // Connects and never sends a line (each would otherwise hold the listener for IO_TIMEOUT).
        let stalled: Vec<TcpStream> = (0..3).map(|_| TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap()).collect();
        let started = Instant::now();
        assert!(matches!(claim(&dir, &["b.psd".into()]), Launch::Forwarded));
        assert!(started.elapsed() < IO_TIMEOUT, "took {:?}", started.elapsed());
        assert_eq!(drain(&primary), vec![OsEvent::Open(vec!["b.psd".into()])]);
        drop(stalled);
        drop(primary);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_token_file_is_private_to_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("mode");
        std::fs::create_dir_all(&dir).unwrap();
        // A wide leftover temporary file from a crashed start doesn't widen the new one.
        let tmp = dir.join(format!("{INFO_FILE}.{}.tmp", std::process::id()));
        std::fs::write(&tmp, "{}").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_info(&dir, 1, &generate_token().unwrap()).unwrap();
        let mode = std::fs::metadata(dir.join(INFO_FILE)).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreachable_instance_falls_back_to_a_standalone_launch() {
        let dir = temp_dir("hung");
        std::fs::create_dir_all(&dir).unwrap();
        // Someone holds the lock but nothing listens on the advertised port.
        let held = File::create(dir.join(LOCK_FILE)).unwrap();
        held.lock().unwrap();
        let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
        write_info(&dir, closed, &generate_token().unwrap()).unwrap();
        assert!(matches!(claim(&dir, &["a.psd".into()]), Launch::Standalone(_)));
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
