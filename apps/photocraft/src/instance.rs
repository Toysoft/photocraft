//! One running PhotoCraft on Windows and Linux.
//!
//! Explorer, a browser and a pinned taskbar shortcut start a new process for each open
//! (`photocraft.exe file.psd`). macOS delivers those opens to the app that is already running
//! (`apple_events`). Here the new process sends its paths to that app and exits, and the running
//! one opens them as documents — tabs, unless Preferences › Workspace › Open Documents as Tabs
//! is off.
//!
//! `instance.json` in the settings directory records the running app (pid, localhost port, token).
//! The first launch listens; a later one connects, sends the paths and leaves. `--control` and
//! `PHOTOCRAFT_NEW_INSTANCE=1` stay their own processes. `--safe-gpu` waits until the process it
//! is replacing has released the record, so a graphics restart does not hand its files to a
//! window that is about to exit.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use photocraft_ui_egui::OsEvent;

const RECORD: &str = "instance.json";
const LOCK: &str = "instance.lock";
/// One line is enough: the Windows command line itself stops near 32 KiB.
const MAX_MESSAGE: u64 = 64 * 1024;
const PROTOCOL: u64 = 1;

/// How this launch relates to an app that is already running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Hand paths to the running app, or become it.
    Singleton,
    /// Become the running app once the previous listener is gone. Never hand paths off.
    Replace,
    /// Don't coordinate. Used for `--control` and `PHOTOCRAFT_NEW_INSTANCE`.
    Solo,
}

/// `control`, paths that aren't valid Unicode, and an explicit separate launch stay out of the
/// running app. A graphics-recovery relaunch replaces it instead of handing work to a process
/// that is exiting.
pub(crate) fn mode(control: bool, safe_gpu: bool, unreadable: bool, separate: bool) -> Mode {
    if control || unreadable || separate {
        Mode::Solo
    } else if safe_gpu {
        Mode::Replace
    } else {
        Mode::Singleton
    }
}

pub(crate) enum Launch {
    /// This process shows the window and accepts later opens.
    Primary(Instance),
    /// A running PhotoCraft accepted the paths. Exit without a window.
    Forwarded,
    /// No coordination (see [`Mode::Solo`]), or the listener could not be set up.
    Solo,
}

/// The listener a primary launch keeps until it exits.
pub(crate) struct Instance {
    shared: Shared,
    shutdown: Arc<AtomicBool>,
    addr: SocketAddr,
    thread: Option<std::thread::JoinHandle<()>>,
    dir: PathBuf,
    pid: u32,
}

/// Cloned into the window so later opens are drained on the UI thread.
#[derive(Clone)]
pub(crate) struct Events {
    shared: Shared,
}

impl Instance {
    pub(crate) fn events(&self) -> Events {
        Events { shared: self.shared.clone() }
    }

    #[cfg(test)]
    fn take_events(&self) -> Vec<OsEvent> {
        self.shared.take()
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(500));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        if read_record(&self.dir).is_some_and(|record| record.pid == self.pid && record.port == self.addr.port()) {
            let _ = std::fs::remove_file(self.dir.join(RECORD));
        }
    }
}

impl Events {
    /// Queue of opens since the last frame. Also wakes the UI when one arrives.
    pub(crate) fn connect(&self, ctx: &egui::Context) -> photocraft_ui_egui::OsEventsFn {
        ctx.request_repaint();
        let ctx = ctx.clone();
        self.shared.set_wake(move || ctx.request_repaint());
        let shared = self.shared.clone();
        Box::new(move || shared.take())
    }
}

/// Become the running app, or hand `paths` to the one that already is.
///
/// `dir` is the settings directory. A missing or unwritable directory starts a normal window
/// ([`Launch::Solo`]) rather than failing the launch.
pub(crate) fn acquire(dir: &Path, paths: &[String], mode: Mode) -> Launch {
    if mode == Mode::Solo {
        return Launch::Solo;
    }
    if let Err(error) = std::fs::create_dir_all(dir) {
        log::warn!("single-instance directory {}: {error}", dir.display());
        return Launch::Solo;
    }
    let paths = absolute_paths(paths);
    match mode {
        Mode::Solo => Launch::Solo,
        Mode::Replace => replace(dir),
        Mode::Singleton => singleton(dir, &paths),
    }
}

fn singleton(dir: &Path, paths: &[String]) -> Launch {
    let Some(guard) = lock(dir) else {
        log::warn!("couldn't lock the single-instance record; opening a separate window");
        return Launch::Solo;
    };
    if let Some(record) = read_record(dir) {
        match forward(&record, paths) {
            Ok(()) => {
                drop(guard);
                return Launch::Forwarded;
            }
            Err(error) => log::info!("the running PhotoCraft didn't take the files ({error}); opening a separate window"),
        }
    }
    match listen(dir) {
        Ok(instance) => Launch::Primary(instance),
        Err(error) => {
            log::warn!("single-instance listener: {error}");
            Launch::Solo
        }
    }
}

/// Wait until whatever is listening lets go (the process `--safe-gpu` is replacing), then listen.
fn replace(dir: &Path) -> Launch {
    for _ in 0..40 {
        let busy = {
            let Some(_guard) = lock(dir) else {
                return Launch::Solo;
            };
            read_record(dir).is_some_and(|record| port_accepts(record.port))
        };
        if !busy {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let Some(_guard) = lock(dir) else {
        return Launch::Solo;
    };
    match listen(dir) {
        Ok(instance) => Launch::Primary(instance),
        Err(error) => {
            log::warn!("single-instance listener: {error}");
            Launch::Solo
        }
    }
}

fn listen(dir: &Path) -> Result<Instance, String> {
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).map_err(|e| e.to_string())?;
    let addr = listener.local_addr().map_err(|e| e.to_string())?;
    let pid = std::process::id();
    let record = Record { pid, port: addr.port(), token: new_token() };
    write_record(dir, &record).map_err(|e| e.to_string())?;
    let shutdown = Arc::new(AtomicBool::new(false));
    let shared = Shared::default();
    let thread_shutdown = Arc::clone(&shutdown);
    let thread_shared = shared.clone();
    let token = record.token.clone();
    let thread = match std::thread::Builder::new().name("photocraft-instance".into()).spawn(move || serve(listener, thread_shutdown, thread_shared, token)) {
        Ok(thread) => thread,
        Err(error) => {
            let _ = std::fs::remove_file(dir.join(RECORD));
            return Err(error.to_string());
        }
    };
    Ok(Instance { shared, shutdown, addr, thread: Some(thread), dir: dir.to_path_buf(), pid })
}

fn serve(listener: TcpListener, shutdown: Arc<AtomicBool>, shared: Shared, token: String) {
    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                if let Err(error) = handle_client(stream, &token, &shared) {
                    log::debug!("single-instance open: {error}");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                if !shutdown.load(Ordering::SeqCst) {
                    log::warn!("single-instance listener stopped: {error}");
                }
                break;
            }
        }
    }
}

fn handle_client(mut stream: TcpStream, token: &str, shared: &Shared) -> Result<(), String> {
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let line = read_line(&mut stream, MAX_MESSAGE)?;
    let paths = match parse_request(&line, token) {
        Ok(paths) => paths,
        Err(error) => {
            let _ = write_reply(&mut stream, false, &error);
            return Err(error);
        }
    };
    shared.push(OsEvent::Open(paths));
    raise_our_windows();
    write_reply(&mut stream, true, "")
}

fn forward(record: &Record, paths: &[String]) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    allow_foreground(record.pid);
    let addr = SocketAddr::from(([127, 0, 0, 1], record.port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(400)).map_err(|e| e.to_string())?;
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut line = serde_json::json!({"v": PROTOCOL, "token": record.token, "paths": paths}).to_string();
    line.push('\n');
    if line.len() as u64 > MAX_MESSAGE {
        return Err("too many paths to hand off".into());
    }
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
    let reply = read_line(&mut stream, MAX_MESSAGE)?;
    let value: serde_json::Value = serde_json::from_str(&reply).map_err(|e| e.to_string())?;
    if value.get("ok").and_then(|ok| ok.as_bool()) == Some(true) {
        Ok(())
    } else {
        Err(value.get("error").and_then(|error| error.as_str()).unwrap_or("rejected").to_string())
    }
}

fn parse_request(line: &str, token: &str) -> Result<Vec<String>, String> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|e| e.to_string())?;
    if value.get("v").and_then(|v| v.as_u64()) != Some(PROTOCOL) {
        return Err("unsupported open request".into());
    }
    if value.get("token").and_then(|token| token.as_str()).unwrap_or("") != token {
        return Err("rejected".into());
    }
    let paths = value.get("paths").and_then(|paths| paths.as_array()).ok_or("missing paths")?;
    if paths.len() > 1024 {
        return Err("too many paths".into());
    }
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let path = path.as_str().ok_or("path is not a string")?;
        if path.len() > 32_768 {
            return Err("path is too long".into());
        }
        if !path.is_empty() {
            out.push(path.to_string());
        }
    }
    Ok(out)
}

fn write_reply(stream: &mut TcpStream, ok: bool, error: &str) -> Result<(), String> {
    let line = if ok { "{\"ok\":true}\n".to_string() } else { format!("{}\n", serde_json::json!({"ok": false, "error": error})) };
    stream.write_all(line.as_bytes()).map_err(|e| e.to_string())
}

/// Byte at a time, so a buffered read can't sit waiting for bytes the sender hasn't written yet
/// (it writes one line, then waits for the reply). The whole line has one deadline: a peer that
/// dribbles a byte at a time must not hold the listener open.
fn read_line(stream: &mut TcpStream, max: u64) -> Result<String, String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if buf.len() as u64 >= max {
            return Err("message too long".into());
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("timed out".into());
        }
        let _ = stream.set_read_timeout(Some(left));
        match stream.read(&mut byte) {
            Ok(0) => return Err(if buf.is_empty() { "connection closed" } else { "message too long" }.into()),
            Ok(_) if byte[0] == b'\n' => break,
            Ok(_) => buf.push(byte[0]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut || error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err("timed out".into());
            }
            Err(error) => return Err(error.to_string()),
        }
    }
    if buf.last() == Some(&b'\r') {
        buf.pop();
    }
    String::from_utf8(buf).map_err(|_| "message is not UTF-8".into())
}

fn absolute_paths(paths: &[String]) -> Vec<String> {
    paths.iter().map(|path| std::path::absolute(path).map(|abs| abs.to_string_lossy().into_owned()).unwrap_or_else(|_| path.clone())).collect()
}

fn port_accepts(port: u16) -> bool {
    TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_millis(200)).is_ok()
}

struct Record {
    pid: u32,
    port: u16,
    token: String,
}

fn read_record(dir: &Path) -> Option<Record> {
    let mut file = std::fs::File::open(dir.join(RECORD)).ok()?;
    let mut buf = Vec::new();
    Read::take(&mut file, 4096).read_to_end(&mut buf).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&buf).ok()?;
    let pid = u32::try_from(value.get("pid")?.as_u64()?).ok()?;
    let port = u16::try_from(value.get("port")?.as_u64()?).ok()?;
    if port == 0 {
        return None;
    }
    let token = value.get("token")?.as_str()?;
    if token.is_empty() || token.len() > 128 {
        return None;
    }
    Some(Record { pid, port, token: token.to_string() })
}

fn write_record(dir: &Path, record: &Record) -> std::io::Result<()> {
    let path = dir.join(RECORD);
    let tmp = dir.join(format!("instance.{}.tmp", std::process::id()));
    let text = serde_json::json!({"pid": record.pid, "port": record.port, "token": record.token}).to_string();
    std::fs::write(&tmp, text.as_bytes())?;
    // The token is the right to open files in this process. Other accounts can connect to
    // 127.0.0.1, so the record must not be world-readable. Windows already confines %APPDATA%.
    restrict_to_user(&tmp);
    let _ = std::fs::remove_file(&path);
    if std::fs::rename(&tmp, &path).is_err() {
        let result = std::fs::write(&path, text.as_bytes());
        restrict_to_user(&path);
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    Ok(())
}

fn restrict_to_user(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
            log::warn!("single-instance record permissions: {error}");
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Serialises "read the record, then listen or forward" so two launches can't both decide
/// they are first. The file holds a nonce so a crashed owner's lock can be removed after a few
/// seconds without a later owner deleting a lock that isn't theirs.
fn lock(dir: &Path) -> Option<Lock> {
    let path = dir.join(LOCK);
    let nonce = new_token();
    let started = Instant::now();
    loop {
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                if file.write_all(nonce.as_bytes()).is_err() {
                    let _ = std::fs::remove_file(&path);
                    return None;
                }
                return Some(Lock { path, nonce });
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path).and_then(|meta| meta.modified()).is_ok_and(stale_since);
                if stale || started.elapsed() > Duration::from_secs(5) {
                    let _ = std::fs::remove_file(&path);
                    continue;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return None,
        }
    }
}

fn stale_since(modified: SystemTime) -> bool {
    modified.elapsed().unwrap_or(Duration::ZERO) > Duration::from_secs(5)
}

struct Lock {
    path: PathBuf,
    nonce: String,
}

impl Drop for Lock {
    fn drop(&mut self) {
        if std::fs::read_to_string(&self.path).ok().as_deref() == Some(self.nonce.as_str()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Unpredictable to another local account: `RandomState::new` draws its keys from the operating
/// system's generator. The record file's permissions are what keep the token off other accounts.
fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let state = std::collections::hash_map::RandomState::new();
    let mut out = String::with_capacity(64);
    for i in 0..4u64 {
        let mut hasher = state.build_hasher();
        hasher.write_u64(i);
        hasher.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out
}

fn raise_our_windows() {
    #[cfg(target_os = "windows")]
    windows::raise_our_windows();
}

#[cfg(target_os = "windows")]
fn allow_foreground(pid: u32) {
    if let Err(error) = winsafe::AllowSetForegroundWindow(Some(pid)) {
        log::debug!("couldn't allow the running PhotoCraft to take focus: {error}");
    }
}

/// The process the shell just started is allowed to take the foreground. It grants that to the
/// running PhotoCraft, which then restores and raises its own window. Without the grant Windows
/// ignores the raise when the open came from Explorer or a browser.
#[cfg(target_os = "windows")]
mod windows {
    pub(super) fn raise_our_windows() {
        let pid = std::process::id();
        let mut top = None;
        let _ = winsafe::EnumWindows(|hwnd| {
            let (_, window_pid) = hwnd.GetWindowThreadProcessId();
            if window_pid == pid && hwnd.IsWindowVisible() {
                if hwnd.IsIconic() {
                    hwnd.ShowWindow(winsafe::co::SW::RESTORE);
                }
                if top.is_none() {
                    top = Some(hwnd);
                }
            }
            true
        });
        if let Some(hwnd) = top {
            hwnd.SetForegroundWindow();
        }
    }
}

#[derive(Clone)]
struct Shared(Arc<Inner>);

struct Inner {
    events: Mutex<Vec<OsEvent>>,
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl Default for Shared {
    fn default() -> Self {
        Self(Arc::new(Inner { events: Mutex::new(Vec::new()), wake: Mutex::new(None) }))
    }
}

impl Shared {
    fn push(&self, event: OsEvent) {
        self.0.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(event);
        if let Some(wake) = self.0.wake.lock().unwrap_or_else(std::sync::PoisonError::into_inner).as_ref() {
            wake();
        }
    }

    fn take(&self) -> Vec<OsEvent> {
        std::mem::take(&mut *self.0.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner))
    }

    fn set_wake(&self, wake: impl Fn() + Send + Sync + 'static) {
        *self.0.wake.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Arc::new(wake));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let dir =
            std::env::temp_dir().join(format!("photocraft-instance-{tag}-{}-{}", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn primary(launch: Launch) -> Instance {
        match launch {
            Launch::Primary(instance) => instance,
            Launch::Forwarded => panic!("expected the first listener, got a handoff"),
            Launch::Solo => panic!("expected the first listener, got a separate window"),
        }
    }

    #[test]
    fn launch_mode_keeps_control_and_recovery_apart() {
        assert_eq!(mode(false, false, false, false), Mode::Singleton);
        assert_eq!(mode(true, true, false, false), Mode::Solo, "a control server is not the user's window");
        assert_eq!(mode(false, false, true, false), Mode::Solo, "a bad path has to be reported by a window");
        assert_eq!(mode(false, false, false, true), Mode::Solo);
        assert_eq!(mode(false, true, false, false), Mode::Replace);
    }

    #[test]
    fn a_second_launch_hands_absolute_paths_to_the_running_app() {
        let dir = temp("forward");
        let running = primary(acquire(&dir, &[], Mode::Singleton));
        let wanted = std::path::absolute("rel-file.psd").unwrap();
        let launch = acquire(&dir, &["rel-file.psd".into()], Mode::Singleton);
        assert!(matches!(launch, Launch::Forwarded));
        assert_eq!(running.take_events(), vec![OsEvent::Open(vec![wanted.to_string_lossy().into_owned()])]);
        drop(running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_launch_with_no_paths_only_asks_the_running_app_forward() {
        let dir = temp("activate");
        let running = primary(acquire(&dir, &[], Mode::Singleton));
        assert!(matches!(acquire(&dir, &[], Mode::Singleton), Launch::Forwarded));
        assert_eq!(running.take_events(), vec![OsEvent::Open(Vec::new())]);
        drop(running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_token_is_refused_and_a_later_open_still_arrives() {
        let dir = temp("token");
        let running = primary(acquire(&dir, &[], Mode::Singleton));
        let record: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join(RECORD)).unwrap()).unwrap();
        let port = u16::try_from(record["port"].as_u64().unwrap()).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        stream.write_all(b"{\"v\":1,\"token\":\"nope\",\"paths\":[\"secret.psd\"]}\n").unwrap();
        let mut buf = String::new();
        Read::take(&mut stream, 1024).read_to_string(&mut buf).unwrap();
        assert!(buf.contains("false"), "{buf}");
        drop(stream);
        assert!(matches!(acquire(&dir, &["ok.psd".into()], Mode::Singleton), Launch::Forwarded));
        let events = running.take_events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            OsEvent::Open(paths) => assert!(paths.len() == 1 && paths[0].ends_with("ok.psd"), "{paths:?}"),
            OsEvent::Quit => panic!("expected an open"),
        }
        drop(running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_malformed_open_is_refused_and_the_listener_stays_up() {
        let dir = temp("garbage");
        let running = primary(acquire(&dir, &[], Mode::Singleton));
        let record: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join(RECORD)).unwrap()).unwrap();
        let port = u16::try_from(record["port"].as_u64().unwrap()).unwrap();
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
        stream.write_all(b"this is not json\n").unwrap();
        let mut buf = String::new();
        Read::take(&mut stream, 1024).read_to_string(&mut buf).unwrap();
        assert!(buf.contains("false"), "{buf}");
        drop(stream);
        assert!(matches!(acquire(&dir, &["ok.psd".into()], Mode::Singleton), Launch::Forwarded));
        assert_eq!(running.take_events().len(), 1);
        drop(running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn the_record_is_unreadable_by_other_accounts() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp("mode");
        let running = primary(acquire(&dir, &[], Mode::Singleton));
        let mode = std::fs::metadata(dir.join(RECORD)).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0, "instance.json mode is {mode:o}");
        drop(running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn quitting_releases_the_record_for_the_next_launch() {
        let dir = temp("release");
        let running = primary(acquire(&dir, &[], Mode::Singleton));
        drop(running);
        assert!(!dir.join(RECORD).exists());
        let next = primary(acquire(&dir, &[], Mode::Singleton));
        drop(next);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_separate_launch_does_not_take_the_running_apps_files() {
        let dir = temp("solo");
        let running = primary(acquire(&dir, &[], Mode::Singleton));
        assert!(matches!(acquire(&dir, &["other.psd".into()], Mode::Solo), Launch::Solo));
        assert!(running.take_events().is_empty());
        drop(running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_launches_at_once_share_one_window() {
        let dir = temp("race");
        let left = dir.clone();
        let right = dir.clone();
        let a = std::thread::spawn(move || acquire(&left, &["a.psd".into()], Mode::Singleton));
        let b = std::thread::spawn(move || acquire(&right, &["b.psd".into()], Mode::Singleton));
        let a = a.join().unwrap();
        let b = b.join().unwrap();
        let (running, forwarded) = match (a, b) {
            (Launch::Primary(running), Launch::Forwarded) => (running, "b.psd"),
            (Launch::Forwarded, Launch::Primary(running)) => (running, "a.psd"),
            (Launch::Primary(_), Launch::Primary(_)) => panic!("both launches opened a window"),
            (Launch::Forwarded, Launch::Forwarded) => panic!("neither launch opened a window"),
            (Launch::Solo, _) | (_, Launch::Solo) => panic!("a launch opted out of the single instance"),
        };
        let events = running.take_events();
        assert_eq!(events.len(), 1, "{events:?}");
        match &events[0] {
            OsEvent::Open(paths) => assert!(paths.len() == 1 && paths[0].ends_with(forwarded), "{paths:?}"),
            OsEvent::Quit => panic!("expected an open"),
        }
        drop(running);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replacing_waits_until_the_previous_listener_is_gone() {
        let dir = temp("replace");
        let running = primary(acquire(&dir, &[], Mode::Singleton));
        let dir_for_replace = dir.clone();
        let replaced = std::thread::spawn(move || acquire(&dir_for_replace, &["kept.psd".into()], Mode::Replace));
        std::thread::sleep(Duration::from_millis(150));
        assert!(running.take_events().is_empty(), "a recovery launch must not hand its files to the window it replaces");
        drop(running);
        let replaced = primary(replaced.join().unwrap());
        assert!(matches!(acquire(&dir, &["next.psd".into()], Mode::Singleton), Launch::Forwarded));
        let events = replaced.take_events();
        match events.first() {
            Some(OsEvent::Open(paths)) => assert!(paths.len() == 1 && paths[0].ends_with("next.psd"), "{paths:?}"),
            other => panic!("expected the new listener to take the next open, got {other:?}"),
        }
        drop(replaced);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
