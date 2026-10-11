//! `anythingd --install | --uninstall | --start | --stop | --restart`:
//! sets up and controls the systemd service. `--install-app` installs
//! Anything for all users, `--update` replaces it with the latest signed
//! release. Run as root, normally through `pkexec` from Anything's settings,
//! which asks for the password.
//!
//! Each action does one fixed thing; nothing here takes paths or other
//! input from the caller.
//!
//! From the Debian package, apt owns the files: only starting, stopping and
//! updating (through apt) are done here.

use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
};

use anything_update::layout::{self, Install, LOCAL};

/// Where the service program is installed by these actions; the unit and
/// the polkit rules point here.
const PROGRAM: &str = LOCAL.service;
const UNIT: &str = LOCAL.unit;
const CONFIG: &str = "/etc/anything/anythingd.toml";
const POLICY: &str = "/usr/share/polkit-1/actions/io.github.srcmkr.anything.policy";
const STATE: &str = "/var/lib/anything";
const SERVICE: &str = "anythingd.service";
/// The app and the command line client, for all users.
const APP: &str = LOCAL.app;
const CLI: &str = LOCAL.cli;
const DESKTOP: &str = "/usr/local/share/applications/anything.desktop";
const ICON: &str = "/usr/local/share/icons/hicolor/scalable/apps/anything.svg";

const UNIT_TEXT: &str = include_str!("../../../packaging/anythingd.service");
const CONFIG_TEXT: &str = include_str!("../../../packaging/anythingd.toml");
const POLICY_TEXT: &str = include_str!("../../../packaging/io.github.srcmkr.anything.policy");
const DESKTOP_TEXT: &str = include_str!("../../../packaging/anything.desktop");
const ICON_TEXT: &str = include_str!("../../anything-gui/assets/anything.svg");
/// Where the Debian package's update is put for apt.
const DOWNLOADS: &str = "/var/cache/anything";

fn unit_text() -> String {
    layout::fill(UNIT_TEXT, &LOCAL)
}

fn policy_text() -> String {
    layout::fill(POLICY_TEXT, &LOCAL)
}

/// Runs `action` if it is one; `None` for any other argument.
pub fn run(action: &str) -> Option<i32> {
    let packaged = Install::current() == Install::Deb;
    let result = match action {
        "--start" | "--stop" | "--restart" | "--update" | "--install" | "--uninstall" | "--install-app"
        | "--refresh-service"
            if packaged =>
        {
            match action {
                "--start" => systemctl(&["enable", "--now", SERVICE]),
                "--stop" => systemctl(&["disable", "--now", SERVICE]),
                "--restart" => systemctl(&["restart", SERVICE]),
                "--update" => update_package(),
                _ => Err(io::Error::other("installed from the Debian package: use apt")),
            }
        }
        "--install" => install(),
        "--uninstall" => uninstall(),
        "--start" => systemctl(&["enable", "--now", SERVICE]),
        "--stop" => systemctl(&["disable", "--now", SERVICE]),
        "--restart" => systemctl(&["restart", SERVICE]),
        "--install-app" => install_app(),
        "--update" => update(),
        // After an update, run by the new program: its unit and rules.
        "--refresh-service" => refresh_service(),
        _ => return None,
    };
    Some(match result {
        Ok(()) => 0,
        Err(err) => {
            eprintln!("anythingd: {err}");
            1
        }
    })
}

fn require_root() -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied, "needs root"));
    }
    Ok(())
}

fn systemctl(args: &[&str]) -> io::Result<()> {
    require_root()?;
    let status = Command::new("systemctl").args(args).status()?;
    if !status.success() {
        return Err(io::Error::other(format!("systemctl {} failed ({status})", args.join(" "))));
    }
    Ok(())
}

/// Writes `text` to `path` through a temporary file, so a running program or
/// a reader never sees half of it.
fn write_file(path: &str, contents: &[u8], mode: u32) -> io::Result<()> {
    let path = Path::new(path);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("new");
    fs::write(&tmp, contents)?;
    fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
    fs::rename(&tmp, path)
}

/// Copies this program into place (or updates it), sets up the unit, the
/// default settings and the polkit rules, and (re)starts the service.
fn install() -> io::Result<()> {
    require_root()?;
    // A service started by hand would keep the socket.
    let manual = !Command::new("systemctl")
        .args(["is-active", "--quiet", SERVICE])
        .status()
        .is_ok_and(|s| s.success())
        && std::os::unix::net::UnixStream::connect(anything_proto::SOCKET).is_ok();
    if manual {
        return Err(io::Error::new(io::ErrorKind::AddrInUse, "a service started by hand is running; stop it first"));
    }
    let me = std::env::current_exe()?;
    if me != Path::new(PROGRAM) {
        write_file(PROGRAM, &fs::read(&me)?, 0o755)?;
    }
    write_file(UNIT, unit_text().as_bytes(), 0o644)?;
    if !Path::new(CONFIG).exists() {
        write_file(CONFIG, CONFIG_TEXT.as_bytes(), 0o644)?;
    }
    write_file(POLICY, policy_text().as_bytes(), 0o644)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["enable", SERVICE])?;
    systemctl(&["restart", SERVICE])
}

/// Stops the service and removes everything `install` put in place, and the
/// snapshot: it holds every user's file names. The settings file stays.
fn uninstall() -> io::Result<()> {
    require_root()?;
    // Not installed (any more) is fine.
    let _ = Command::new("systemctl").args(["disable", "--now", SERVICE]).status();
    // Installed for all users, the app still needs this program and its
    // rules for updates.
    let app = Path::new(APP).exists();
    for file in [UNIT, POLICY] {
        if app && file == POLICY {
            continue;
        }
        match fs::remove_file(file) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
    }
    let program_dir = Path::new(PROGRAM).parent().unwrap();
    for dir in [program_dir, Path::new(STATE)] {
        if app && dir == program_dir {
            continue;
        }
        match fs::remove_dir_all(dir) {
            Err(err) if err.kind() != io::ErrorKind::NotFound => return Err(err),
            _ => {}
        }
    }
    systemctl(&["daemon-reload"])
}

/// Installs the three programs lying next to this one, as they come out of
/// `cargo build`, for all users, with a menu entry.
fn install_app() -> io::Result<()> {
    require_root()?;
    let me = std::env::current_exe()?;
    let dir = me.parent().ok_or_else(|| io::Error::other("no folder"))?;
    let read = |name: &str| fs::read(dir.join(name));
    let programs = [read("anything")?, fs::read(&me)?, read("any")?];
    place(&programs)?;
    write_file(POLICY, policy_text().as_bytes(), 0o644)
}

/// Writes the programs, menu entry and icon: all files first under a
/// temporary name, then renamed, so there is never half a version.
fn place(programs: &[Vec<u8>; 3]) -> io::Result<()> {
    let desktop = layout::fill(DESKTOP_TEXT, &LOCAL).into_bytes();
    let files: [(&str, &[u8], u32); 5] = [
        (APP, &programs[0], 0o755),
        (PROGRAM, &programs[1], 0o755),
        (CLI, &programs[2], 0o755),
        (DESKTOP, &desktop, 0o644),
        (ICON, ICON_TEXT.as_bytes(), 0o644),
    ];
    let mut staged = Vec::new();
    for (path, contents, mode) in files {
        let path = Path::new(path);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("new");
        fs::write(&tmp, contents)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        staged.push((tmp, path));
    }
    for (tmp, path) in staged {
        fs::rename(tmp, path)?;
    }
    Ok(())
}

/// Replaces the installed Anything with the latest release, if it is newer
/// and signed with the release key. Takes no input: it finds, downloads and
/// checks the release itself.
fn update() -> io::Result<()> {
    require_root()?;
    use anything_update as up;
    let current = env!("CARGO_PKG_VERSION");
    let release = up::latest().ok_or_else(|| io::Error::other("no release information from GitHub"))?;
    if !up::is_newer(&release.version, current) {
        return Err(io::Error::other(format!("{} is not newer than {current}", release.version)));
    }
    let archive = up::fetch_verified(&release, &up::archive_name(&release.version)).map_err(io::Error::other)?;
    let programs = up::unpack(&archive).map_err(io::Error::other)?;
    place(&programs)?;
    // The new service program sets up its own unit and rules.
    if Path::new(UNIT).exists() {
        let status = Command::new(PROGRAM).arg("--refresh-service").status()?;
        if !status.success() {
            return Err(io::Error::other("the new service did not start"));
        }
    } else {
        write_file(POLICY, policy_text().as_bytes(), 0o644)?;
    }
    println!("{}", release.version);
    Ok(())
}

/// Installed from the Debian package: installs the latest release's package
/// with apt, if it is newer and signed with the release key. Its scripts
/// restart the service.
fn update_package() -> io::Result<()> {
    require_root()?;
    use anything_update as up;
    let current = env!("CARGO_PKG_VERSION");
    let release = up::latest().ok_or_else(|| io::Error::other("no release information from GitHub"))?;
    if !up::is_newer(&release.version, current) {
        return Err(io::Error::other(format!("{} is not newer than {current}", release.version)));
    }
    let name = up::deb_name(&release.version);
    let package = up::fetch_verified(&release, &name).map_err(io::Error::other)?;
    // Readable for apt's download user, written by root only.
    fs::create_dir_all(DOWNLOADS)?;
    fs::set_permissions(DOWNLOADS, fs::Permissions::from_mode(0o755))?;
    let file = Path::new(DOWNLOADS).join(&name);
    write_file(file.to_str().unwrap_or_default(), &package, 0o644)?;
    let status = Command::new("apt-get")
        .args(["install", "-y", "--no-install-recommends"])
        .arg(&file)
        .env("DEBIAN_FRONTEND", "noninteractive")
        .status();
    fs::remove_file(&file).ok();
    if !status?.success() {
        return Err(io::Error::other("apt-get could not install the package"));
    }
    println!("{}", release.version);
    Ok(())
}

/// Writes this version's unit and rules and restarts the service if it runs.
fn refresh_service() -> io::Result<()> {
    require_root()?;
    write_file(UNIT, unit_text().as_bytes(), 0o644)?;
    write_file(POLICY, policy_text().as_bytes(), 0o644)?;
    systemctl(&["daemon-reload"])?;
    systemctl(&["try-restart", SERVICE])
}
