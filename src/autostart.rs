//! "Start at login": Bubo starts hidden (`--hidden`) so notifications arrive without opening it.
//! Asked of the desktop's Background portal first, which writes the autostart entry itself on
//! GNOME and KDE. Where there is no such portal (Hyprland and other compositors' backends don't
//! offer it): with a systemd user session that reaches graphical-session.target, the user unit
//! install.sh puts in place is enabled, so a crash restarts Bubo. Otherwise an XDG autostart
//! entry is written, which every desktop that runs autostart entries starts once.
use gtk4::gio;
use gtk4::glib::{self, prelude::*};
use crate::APP_ID;
use std::path::{Path, PathBuf};

const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const BACKGROUND_INTERFACE: &str = "org.freedesktop.portal.Background";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const SYSTEMD_NAME: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const SESSION_TARGET: &str = "graphical-session.target";

fn unit_name() -> String { format!("{APP_ID}.service") }

/// How "Start at login" was set, for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method { Portal, Systemd, AutostartEntry }

/// What the Background portal made of the request.
enum PortalAnswer {
    /// Starting at login is now on (or off): the portal's word, which may differ from the ask.
    Set(bool),
    /// The user said no in the portal's dialog; nothing changed.
    Declined,
    /// No portal, no Background interface, or it failed: do it ourselves.
    Unavailable(String),
}

/// Turn starting at login on or off. Answers whether Bubo will now start at login.
pub async fn set(is_wanted: bool) -> Result<(bool, Method), String> {
    let bus = gio::bus_get_future(gio::BusType::Session).await.map_err(|e| format!("session bus: {e}"))?;
    let exe = std::env::current_exe().map_err(|e| format!("own path: {e}"))?;
    match request_background(&bus, &exe, is_wanted).await {
        PortalAnswer::Set(is_enabled) => return Ok((is_enabled, Method::Portal)),
        PortalAnswer::Declined => return Err("declined in the portal's dialog".into()),
        PortalAnswer::Unavailable(why) => tracing::info!("background portal unavailable ({why}); setting start at login ourselves"),
    }
    if is_systemd_usable(&bus).await {
        set_unit_enabled(&bus, is_wanted).await.map_err(|e| format!("systemd: {e}"))?;
        return Ok((is_wanted, Method::Systemd));
    }
    set_autostart_entry(&exe, is_wanted).map_err(|e| format!("autostart entry: {e}"))?;
    Ok((is_wanted, Method::AutostartEntry))
}

async fn request_background(bus: &gio::DBusConnection, exe: &Path, is_wanted: bool) -> PortalAnswer {
    // The answer comes back as a signal on a path derived from the token, so subscribe before
    // asking: the portal may answer at once.
    let token = format!("bubo_{}", glib::uuid_string_random().replace('-', "_"));
    let sender = bus.unique_name().map(|n| n.trim_start_matches(':').replace('.', "_")).unwrap_or_default();
    let request_path = format!("{PORTAL_PATH}/request/{sender}/{token}");
    let (tx, rx) = async_channel::bounded::<(u32, glib::Variant)>(1);
    #[allow(deprecated)]
    let subscription = bus.signal_subscribe(Some(PORTAL_NAME), Some(REQUEST_INTERFACE), Some("Response"), Some(&request_path), None, gio::DBusSignalFlags::NONE, move |_, _, _, _, _, params| {
        let _ = tx.try_send((params.child_value(0).get::<u32>().unwrap_or(2), params.child_value(1)));
    });
    let options = glib::VariantDict::new(None);
    options.insert("handle_token", &token);
    options.insert("reason", "Bubo shows new messages as soon as you log in.");
    options.insert("autostart", is_wanted);
    // Becomes the Exec line of the entry the portal writes. Absolute, as launchers often lack ~/.local/bin.
    options.insert("commandline", vec![exe.to_string_lossy().into_owned(), "--hidden".to_string()]);
    let params = glib::Variant::tuple_from_iter(["".to_variant(), options.end()]);
    let call = bus.call_future(Some(PORTAL_NAME), PORTAL_PATH, BACKGROUND_INTERFACE, "RequestBackground", Some(&params), None, gio::DBusCallFlags::NONE, -1).await;
    let answer = match call { Ok(_) => rx.recv().await.ok(), Err(e) => { #[allow(deprecated)] bus.signal_unsubscribe(subscription); return PortalAnswer::Unavailable(e.to_string()); } };
    #[allow(deprecated)]
    bus.signal_unsubscribe(subscription);
    match answer {
        Some((0, results)) => PortalAnswer::Set(glib::VariantDict::new(Some(&results)).lookup::<bool>("autostart").ok().flatten().unwrap_or(false)),
        Some((1, _)) => PortalAnswer::Declined,
        Some((code, _)) => PortalAnswer::Unavailable(format!("response {code}")),
        None => PortalAnswer::Unavailable("no response".into()),
    }
}

/// The user unit is installed and the session reaches the target it hangs off; without either,
/// enabling it would never start anything.
async fn is_systemd_usable(bus: &gio::DBusConnection) -> bool {
    if call(bus, SYSTEMD_PATH, MANAGER_INTERFACE, "GetUnitFileState", (unit_name(),).to_variant()).await.is_err() { return false; }
    let Ok(target) = call(bus, SYSTEMD_PATH, MANAGER_INTERFACE, "GetUnit", (SESSION_TARGET,).to_variant()).await else { return false };
    let Some(path) = target.child_value(0).str().map(str::to_string) else { return false };
    let Ok(state) = call(bus, &path, "org.freedesktop.DBus.Properties", "Get", ("org.freedesktop.systemd1.Unit", "ActiveState").to_variant()).await else { return false };
    // Get answers (v): the variant holds the string.
    state.child_value(0).as_variant().and_then(|v| v.str().map(|s| s == "active")).unwrap_or(false)
}

async fn set_unit_enabled(bus: &gio::DBusConnection, is_wanted: bool) -> Result<(), glib::Error> {
    let units = vec![unit_name()];
    // (files, runtime, force): persistent, and not over someone else's link.
    if is_wanted { call(bus, SYSTEMD_PATH, MANAGER_INTERFACE, "EnableUnitFiles", (units, false, false).to_variant()).await?; }
    else { call(bus, SYSTEMD_PATH, MANAGER_INTERFACE, "DisableUnitFiles", (units, false).to_variant()).await?; }
    // What `systemctl enable` does after linking, so the target sees it.
    call(bus, SYSTEMD_PATH, MANAGER_INTERFACE, "Reload", ().to_variant()).await?;
    Ok(())
}

async fn call(bus: &gio::DBusConnection, path: &str, interface: &str, method: &str, params: glib::Variant) -> Result<glib::Variant, glib::Error> {
    bus.call_future(Some(SYSTEMD_NAME), path, interface, method, Some(&params), None, gio::DBusCallFlags::NONE, -1).await
}

/// Where the desktop looks for autostart entries.
fn autostart_entry_path() -> PathBuf { glib::user_config_dir().join("autostart").join(format!("{APP_ID}.desktop")) }

fn set_autostart_entry(exe: &Path, is_wanted: bool) -> std::io::Result<()> {
    let path = autostart_entry_path();
    if !is_wanted {
        return match std::fs::remove_file(&path) { Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e), _ => Ok(()) };
    }
    if let Some(dir) = path.parent() { std::fs::create_dir_all(dir)?; }
    std::fs::write(&path, autostart_entry(exe))
}

/// The entry for `exe`, quoted as the Desktop Entry spec asks when the path has a character the
/// Exec line would otherwise split or expand on.
fn autostart_entry(exe: &Path) -> String {
    let exe = exe.to_string_lossy();
    let exec = if exe.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) { exe.into_owned() } else {
        let mut quoted = String::from('"');
        for c in exe.chars() {
            // Escaped once for the Exec quoting, then once more as a desktop-file string value.
            if matches!(c, '"' | '`' | '$' | '\\') { quoted.push_str("\\\\"); }
            quoted.push(c);
        }
        quoted.push('"');
        quoted
    };
    format!("[Desktop Entry]\nType=Application\nName=Bubo\nIcon={APP_ID}\nExec={exec} --hidden\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn autostart_entry_quotes_only_what_needs_it() {
        assert!(autostart_entry(Path::new("/home/ada/.local/bin/bubo")).contains("\nExec=/home/ada/.local/bin/bubo --hidden\n"));
        assert!(autostart_entry(Path::new("/opt/my apps/bubo")).contains("\nExec=\"/opt/my apps/bubo\" --hidden\n"));
        assert!(autostart_entry(Path::new("/opt/$x/bubo")).contains("\nExec=\"/opt/\\\\$x/bubo\" --hidden\n"));
    }
}
