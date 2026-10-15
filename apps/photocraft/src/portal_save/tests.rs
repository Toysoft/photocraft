use super::*;
use std::sync::{Arc, Mutex};

#[test]
fn file_uris_become_local_paths() {
    assert_eq!(file_uri_path("file:///home/me/Untitled-1.psd"), Some(PathBuf::from("/home/me/Untitled-1.psd")));
    assert_eq!(file_uri_path("file:///home/me/A%20b%C3%A9.png"), Some(PathBuf::from("/home/me/A bé.png")));
    assert_eq!(file_uri_path("file://localhost/tmp/x.png"), Some(PathBuf::from("/tmp/x.png")));
    assert_eq!(file_uri_path("sftp://host/x.png"), None);
    assert_eq!(file_uri_path("file://host/x.png"), None);
}

#[test]
fn x11_windows_are_named_to_the_portal() {
    use eframe::wgpu::rwh::{RawWindowHandle, XlibWindowHandle};
    assert_eq!(parent_id(Some(RawWindowHandle::Xlib(XlibWindowHandle::new(0x3a0000f)))), "x11:3a0000f");
    assert_eq!(parent_id(None), "");
}

/// What the mock portal answers to one `SaveFile` call: the response code, and the file and
/// filter label the user "picked".
type Script = Vec<(u32, Option<(&'static str, &'static str)>)>;

/// The options of each `SaveFile` call the mock portal received: current_name and the label of
/// current_filter.
type Seen = Arc<Mutex<Vec<(Option<String>, Option<String>)>>>;

/// A private session bus (`dbus-daemon`) with a mock `org.freedesktop.portal.FileChooser` that
/// answers `SaveFile` from `script`. `None` when there is no `dbus-daemon` to run.
fn mock_portal(script: Script) -> Option<(zbus::blocking::Connection, Seen, std::process::Child)> {
    use std::io::BufRead;
    let mut daemon =
        std::process::Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address=1"]).stdout(std::process::Stdio::piped()).spawn().ok()?;
    let mut address = String::new();
    std::io::BufReader::new(daemon.stdout.take()?).read_line(&mut address).ok()?;
    let address = address.trim().to_string();
    let server = zbus::blocking::connection::Builder::address(address.as_str()).unwrap().name("org.freedesktop.portal.Desktop").unwrap().build().unwrap();
    let seen: Seen = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        let mut script = script.into_iter();
        for msg in zbus::blocking::MessageIterator::from(&server) {
            let Ok(msg) = msg else { break };
            let header = msg.header();
            if header.message_type() != zbus::message::Type::MethodCall || header.member().map(|m| m.as_str()) != Some("SaveFile") {
                continue;
            }
            let (_parent, _title, options): (String, String, HashMap<String, OwnedValue>) = msg.body().deserialize().unwrap();
            let text = |key: &str| options.get(key).and_then(|v| String::try_from(v.try_clone().ok()?).ok());
            let filter = options.get("current_filter").and_then(|v| {
                let (label, _): (String, Vec<(u32, String)>) = v.try_clone().ok()?.try_into().ok()?;
                Some(label)
            });
            log.lock().unwrap().push((text("current_name"), filter));
            let sender = header.sender().unwrap().as_str().trim_start_matches(':').replace('.', "_");
            let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{}", text("handle_token").unwrap());
            server.reply(&header, &OwnedObjectPath::try_from(path.as_str()).unwrap()).unwrap();
            let (code, pick) = script.next().unwrap_or((2, None));
            let mut results: HashMap<&str, Value<'_>> = HashMap::new();
            if let Some((uri, label)) = pick {
                results.insert("uris", Value::from(vec![uri.to_string()]));
                results.insert("current_filter", Value::from((label.to_string(), vec![(0u32, "*.x".to_string())])));
            }
            server
                .emit_signal(
                    header.sender().cloned().map(zbus::names::BusName::from),
                    path.as_str(),
                    "org.freedesktop.portal.Request",
                    "Response",
                    &(code, results),
                )
                .unwrap();
        }
    });
    let client = zbus::blocking::connection::Builder::address(address.as_str()).unwrap().build().unwrap();
    Some((client, seen, daemon))
}

fn request(name: &str) -> SaveRequest {
    let filters = [("PSD Document", &["psd", "psb"][..]), ("PNG", &["png"]), ("JPEG", &["jpg"])];
    SaveRequest {
        parent: String::new(),
        name: Some(name.into()),
        folder: Some(PathBuf::from("/tmp")),
        filters: filters.iter().map(|(l, e)| (l.to_string(), e.iter().map(|s| s.to_string()).collect())).collect(),
        current: Some(0),
    }
}

/// #6121 end to end through a real D-Bus: the name comes back as typed with the type picked,
/// and Save As writes it with that type's extension.
#[test]
fn save_as_follows_the_file_type_picked_in_the_portal_dialog() {
    let dir = std::env::temp_dir().join(format!("photocraft-portal-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let typed = format!("file://{}/Untitled-1.psd", dir.display());
    let typed: &'static str = Box::leak(typed.into_boxed_str());
    let Some((conn, seen, mut daemon)) = mock_portal(vec![(0, Some((typed, "PNG"))), (1, None), (0, Some((typed, "PSD Document")))]) else {
        eprintln!("no dbus-daemon: skipped");
        return;
    };
    // PNG picked with `Untitled-1.psd` still in the name: written as Untitled-1.png.
    assert_eq!(save_as(&conn, request("Untitled-1.psd")).unwrap(), Some(dir.join("Untitled-1.png")));
    assert_eq!(seen.lock().unwrap()[0], (Some("Untitled-1.psd".into()), Some("PSD Document".into())), "the request carried the name and type");
    // Cancel.
    assert_eq!(save_as(&conn, request("Untitled-1.psd")).unwrap(), None);
    // The type the name already has: unchanged.
    assert_eq!(save_as(&conn, request("Untitled-1.psd")).unwrap(), Some(dir.join("Untitled-1.psd")));
    daemon.kill().ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// When the extension the type gives names a file that exists, the dialog opens again under that
/// name (so the portal asks before replacing it) instead of overwriting it unasked.
#[test]
fn a_renamed_file_that_exists_is_confirmed_in_the_dialog() {
    let dir = std::env::temp_dir().join(format!("photocraft-portal-exists-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("art.png"), b"old").unwrap();
    let typed: &'static str = Box::leak(format!("file://{}/art.psd", dir.display()).into_boxed_str());
    let confirmed: &'static str = Box::leak(format!("file://{}/art.png", dir.display()).into_boxed_str());
    let Some((conn, seen, mut daemon)) = mock_portal(vec![(0, Some((typed, "PNG"))), (0, Some((confirmed, "PNG")))]) else {
        eprintln!("no dbus-daemon: skipped");
        return;
    };
    assert_eq!(save_as(&conn, request("art.psd")).unwrap(), Some(dir.join("art.png")));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2, "asked again: {seen:?}");
    assert_eq!(seen[1], (Some("art.png".into()), Some("PNG".into())), "under the new name, on the picked type");
    assert_eq!(std::fs::read(dir.join("art.png")).unwrap(), b"old", "nothing written by the dialog");
    daemon.kill().ok();
    std::fs::remove_dir_all(&dir).ok();
}

/// With only one type offered (the Swatches panel's .aco) a typed extension is kept.
#[test]
fn a_single_file_type_keeps_the_typed_extension() {
    let typed: &'static str = "file:///tmp/photocraft-none/Swatches.ase";
    let Some((conn, _seen, mut daemon)) = mock_portal(vec![(0, Some((typed, "Color Swatches")))]) else {
        eprintln!("no dbus-daemon: skipped");
        return;
    };
    let req = SaveRequest { filters: vec![("Color Swatches".into(), vec!["aco".into()])], ..request("Swatches.aco") };
    assert_eq!(save_as(&conn, req).unwrap(), Some(PathBuf::from("/tmp/photocraft-none/Swatches.ase")));
    daemon.kill().ok();
}
