//! Live proof that the tray backend really registers on D-Bus.
//!
//! The unit tests in `src/tray.rs` cover the *policy* (menu ids, close rule) and
//! the icon pixels; they say nothing about whether the icon ever reaches a
//! panel.  This test closes that gap **without a desktop**:
//!
//! 1. it starts a private `dbus-daemon --session` (no display, no panel needed);
//! 2. first it checks the safe case — a session bus with **no**
//!    `org.kde.StatusNotifierWatcher` must yield `available() == false`, so
//!    closing the window quits instead of hiding the app forever;
//! 3. then it serves a minimal fake watcher (what KDE's plasmashell / waybar /
//!    `gnome-shell-extension-appindicator` provide), calls
//!    [`holaf_ui::tray::install`] again and asserts the item **registered** and
//!    exposes the Qt menu (Show/Hide, Restart audio engine, Quit) through
//!    `com.canonical.dbusmenu`.
//!
//! It is skipped (not failed) where `dbus-daemon` is not installed.  Nothing
//! here needs the `tray-gtk` backend: it is ksni/D-Bus only.
#![cfg(feature = "tray")]

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;


/// A private session bus, killed when the guard drops.
struct SessionBus {
    child: Child,
    address: String,
}

impl SessionBus {
    /// Start `dbus-daemon --session` and export its address to this process.
    ///
    /// `None` when the binary is missing (then the test is skipped): building a
    /// bus by hand is not what this test is about.
    fn start() -> Option<Self> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--print-address", "--nofork"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut address = String::new();
        BufReader::new(child.stdout.take()?)
            .read_line(&mut address)
            .ok()?;
        let address = address.trim().to_string();
        if address.is_empty() {
            let _ = child.kill();
            return None;
        }
        // Everything below (our probe and `ksni` alike) reads this variable.
        std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &address);
        Some(Self { child, address })
    }
}

impl Drop for SessionBus {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The smallest `org.kde.StatusNotifierWatcher` that a StatusNotifierItem can
/// register with: it records what registered and answers the two properties the
/// protocol defines.
#[derive(Default)]
struct FakeWatcher {
    registered: Arc<Mutex<Vec<String>>>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl FakeWatcher {
    fn register_status_notifier_item(&self, service: &str) {
        self.registered
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .push(service.to_string());
    }

    fn register_status_notifier_host(&self, _service: &str) {}

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.registered
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }
}

/// Read the top-level labels of the menu `ksni` published on `destination`.
///
/// Reads the raw `com.canonical.dbusmenu` protocol — `GetLayout` returning
/// `(u revision, (i id, a{sv} properties, av children))` — so the assertion is
/// literally what a panel parses to draw the menu.
fn menu_labels(destination: &str) -> Vec<String> {
    use zbus::zvariant::{Dict, ObjectPath, OwnedValue, Structure, Value};

    /// `(id, properties, children)` of one menu node.
    type Node = (
        i32,
        std::collections::HashMap<String, OwnedValue>,
        Vec<OwnedValue>,
    );

    /// A dbusmenu property is a variant (`v`): unwrap it before reading.
    fn unvariant<'a>(value: &'a Value<'a>) -> &'a Value<'a> {
        match value {
            Value::Value(inner) => inner.as_ref(),
            other => other,
        }
    }

    let connection = zbus::blocking::Connection::session().expect("session bus");
    let properties = zbus::blocking::Proxy::new(
        &connection,
        destination,
        "/StatusNotifierItem",
        "org.freedesktop.DBus.Properties",
    )
    .expect("properties proxy");
    // The item object carries the path of its own dbusmenu.
    let menu: OwnedValue = properties
        .call("Get", &("org.kde.StatusNotifierItem", "Menu"))
        .expect("the item must expose a menu");
    let menu_path = <&ObjectPath>::try_from(&menu)
        .expect("the menu path is an object path")
        .to_string();

    let menu = zbus::blocking::Proxy::new(
        &connection,
        destination,
        menu_path.as_str(),
        "com.canonical.dbusmenu",
    )
    .expect("menu proxy");
    let message = menu
        .call_method("GetLayout", &(0i32, -1i32, Vec::<String>::new()))
        .expect("GetLayout must answer");
    let body = message.body();
    let (_revision, (_id, _properties, children)): (u32, Node) =
        body.deserialize().expect("layout body");

    children
        .into_iter()
        .filter_map(|child| {
            // Each child is `(id, properties, children)`.
            let child: Value<'_> = child.into();
            let node: &Structure = <&Structure>::try_from(&child).ok()?;
            let properties: &Dict = <&Dict>::try_from(&node.fields()[1]).ok()?;
            let label = properties.iter().find_map(|(key, value)| {
                if <&str>::try_from(key).ok()? == "label" {
                    <&str>::try_from(unvariant(value)).ok().map(str::to_string)
                } else {
                    None
                }
            });
            label
        })
        .collect()
}

#[test]
fn the_ksni_backend_registers_on_a_real_session_bus() {
    let Some(bus) = SessionBus::start() else {
        eprintln!("SKIP: dbus-daemon is not installed, cannot run a private session bus");
        return;
    };
    // Sanity: the address really is a bus we can talk to.
    assert!(
        zbus::blocking::Connection::session().is_ok(),
        "the private bus at {} must be reachable",
        bus.address
    );

    // --- Case 1: a bus with no StatusNotifier host -------------------------
    // `tray-icon` would happily claim success here (it spawns with
    // `assume_sni_available(true)`); our probe must not, or "close hides the
    // window" would strand the app with no icon to bring it back.
    holaf_ui::tray::install();
    assert!(
        !holaf_ui::tray::available(),
        "no watcher on the bus ⇒ no tray, and closing must quit"
    );
    assert_eq!(
        holaf_ui::tray::close_action(holaf_ui::tray::available(), true),
        holaf_ui::tray::CloseAction::Quit
    );

    // --- Case 2: a real watcher -------------------------------------------
    let watcher = FakeWatcher::default();
    let registered = Arc::clone(&watcher.registered);
    let _service = zbus::blocking::connection::Builder::session()
        .expect("session bus")
        .name("org.kde.StatusNotifierWatcher")
        .expect("valid bus name")
        .serve_at("/StatusNotifierWatcher", watcher)
        .expect("serve the watcher")
        .build()
        .expect("the watcher service must start");

    holaf_ui::tray::install();
    assert!(
        holaf_ui::tray::available(),
        "with a StatusNotifier host the tray must report itself available"
    );
    assert_eq!(
        holaf_ui::tray::close_action(holaf_ui::tray::available(), true),
        holaf_ui::tray::CloseAction::Hide,
        "with a tray, closing hides the window (Qt behaviour)"
    );

    // The item registered itself exactly once, and points at a real object.
    let destination = {
        let mut registered = registered
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone();
        assert_eq!(registered.len(), 1, "one StatusNotifierItem: {registered:?}");
        registered.remove(0)
    };
    assert!(!destination.is_empty(), "a registration must name the item");

    // And the menu it published is the Qt one, in the Qt order.  The snapshot is
    // published by a tray-icon watcher thread, so poll instead of racing it.
    let mut attempts = 0;
    let labels = loop {
        let labels = menu_labels(&destination);
        if !labels.is_empty() || attempts >= 50 {
            break labels;
        }
        attempts += 1;
        std::thread::sleep(Duration::from_millis(20));
    };
    let visible: Vec<&str> = labels
        .iter()
        .map(String::as_str)
        .filter(|label| !label.is_empty())
        .collect();
    assert_eq!(
        visible,
        ["Show/Hide", "Restart audio engine", "Quit"],
        "the tray menu must match the Qt one (labels: {labels:?})"
    );
}
