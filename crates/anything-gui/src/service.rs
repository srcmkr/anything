//! The system service as the settings show it: whether it is installed and
//! runs, and installing, starting, stopping and removing it. Changes go
//! through `pkexec`, which asks for an administrator password; it runs the
//! service program with one fixed argument (see `anythingd --install`).

use std::{
    io::Read as _,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Mutex,
    time::SystemTime,
};

use anything_proto::{Client, Request, Response, State};

use anything_update::layout;

/// The installed service program, from the Debian package or `/usr/local`;
/// the polkit rules name it.
fn installed_program() -> &'static str {
    layout::service_paths().service
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Info {
    pub installed: bool,
    /// Switched on: runs now and after every boot.
    pub enabled: bool,
    /// systemd's view: `active`, `activating`, `failed`, `inactive`, …
    pub active_state: String,
    /// What the service itself says, if it answers.
    pub answer: Option<(State, u64)>,
    /// The service program that comes with this Anything.
    pub bundled: Option<PathBuf>,
    /// The installed program is not the one that comes with this Anything.
    pub outdated: bool,
    /// Installed by the Debian package: apt installs and removes it.
    pub packaged: bool,
}

impl Info {
    /// Answers, but systemd does not run it: started by hand.
    pub fn manual(&self) -> bool {
        self.answer.is_some() && self.active_state != "active"
    }
}

/// Reads the state; blocks for a few milliseconds.
pub fn probe() -> Info {
    let paths = layout::service_paths();
    let packaged = *paths == layout::DEB;
    let installed = Path::new(paths.unit).exists();
    let (mut enabled, mut active_state) = (false, String::from("inactive"));
    if installed
        && let Ok(out) = Command::new("systemctl")
            .args(["show", "--property=UnitFileState,ActiveState", "anythingd.service"])
            .stderr(Stdio::null())
            .output()
    {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            match line.split_once('=') {
                Some(("UnitFileState", value)) => enabled = value == "enabled",
                Some(("ActiveState", value)) => active_state = value.to_owned(),
                _ => {}
            }
        }
    }
    let answer = Client::connect(&crate::source::service_socket()).ok().and_then(|mut client| {
        match client.ask(&Request::Status) {
            Ok(Response::Status { state, entries, .. }) => Some((state, entries)),
            _ => None,
        }
    });
    let bundled = bundled();
    let outdated = installed && !packaged && bundled.as_deref().is_some_and(|b| differs(b, Path::new(installed_program())));
    Info { installed, enabled, active_state, answer, bundled, outdated, packaged }
}

/// `anythingd` next to this program, as both are built and packaged.
pub fn bundled() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let path = exe.with_file_name("anythingd");
    path.is_file().then_some(path)
}

/// Whether two programs differ. Compared byte by byte, but only again when
/// one of them changed on disk.
fn differs(a: &Path, b: &Path) -> bool {
    type Stamp = Option<(u64, SystemTime)>;
    static LAST: Mutex<Option<((Stamp, Stamp), bool)>> = Mutex::new(None);
    let stamp = |p: &Path| -> Stamp {
        let meta = std::fs::metadata(p).ok()?;
        Some((meta.len(), meta.modified().ok()?))
    };
    let key = (stamp(a), stamp(b));
    let mut last = LAST.lock().unwrap();
    if let Some((known, result)) = *last
        && known == key
    {
        return result;
    }
    let result = match (std::fs::read(a), std::fs::read(b)) {
        (Ok(x), Ok(y)) => x != y,
        _ => true,
    };
    *last = Some((key, result));
    result
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Install,
    Uninstall,
    Start,
    Stop,
    Restart,
    /// Installs this build's three programs for all users.
    InstallApp,
    /// Replaces the installed Anything with the latest signed release.
    Update,
}

impl Action {
    fn argument(self) -> &'static str {
        match self {
            Action::Install => "--install",
            Action::Uninstall => "--uninstall",
            Action::Start => "--start",
            Action::Stop => "--stop",
            Action::Restart => "--restart",
            Action::InstallApp => "--install-app",
            Action::Update => "--update",
        }
    }

    /// The program to run: installing takes the one that comes with this
    /// Anything; the rest the installed one, which the polkit rules (with
    /// their friendly password prompt) are for.
    fn program(self, info: &Info) -> Option<PathBuf> {
        let installed = Path::new(installed_program()).is_file().then(|| PathBuf::from(installed_program()));
        match self {
            Action::Install => info.bundled.clone().or(installed),
            Action::InstallApp => info.bundled.clone(),
            // Only the installed program updates, and only itself.
            Action::Update => installed,
            _ => installed.or_else(|| info.bundled.clone()),
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
pub enum Outcome {
    Done,
    /// The password dialog was closed.
    Cancelled,
    /// Wrong password or not allowed.
    Refused,
    /// No way to ask for the password here; the command to run by hand.
    NoPrompt(String),
    Failed(String),
}

/// Runs `action` as root; blocks until the password was given and the
/// action is done.
pub fn run(action: Action, info: &Info) -> Outcome {
    let Some(program) = action.program(info) else {
        return Outcome::Failed("anythingd not found".into());
    };
    // Inside a running AppImage the program lies in a mount only this user
    // may enter, not root: hand root a copy.
    let copy = match layout::Install::current() {
        layout::Install::AppImage(_) if !program.starts_with("/usr/") => match copy_out(&program) {
            Ok(copy) => Some(copy),
            Err(error) => return Outcome::Failed(error.to_string()),
        },
        _ => None,
    };
    let program = copy.as_ref().map_or(program, |c| c.join("anythingd"));
    let outcome = run_program(&program, action);
    if let Some(dir) = copy {
        std::fs::remove_dir_all(dir).ok();
    }
    outcome
}

/// Copies `program` into a new private folder in the temporary directory.
fn copy_out(program: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let dir = std::env::temp_dir().join(format!("anything-install-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
    let copy = dir.join("anythingd");
    std::fs::copy(program, &copy)?;
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755))?;
    Ok(dir)
}

fn run_program(program: &Path, action: Action) -> Outcome {
    let by_hand = format!("sudo {} {}", program.display(), action.argument());
    let child = Command::new("pkexec")
        .arg(program)
        .arg(action.argument())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let Ok(mut child) = child else { return Outcome::NoPrompt(by_hand) };
    let mut errors = String::new();
    if let Some(mut stderr) = child.stderr.take() {
        stderr.read_to_string(&mut errors).ok();
    }
    let Ok(status) = child.wait() else { return Outcome::Failed("pkexec".into()) };
    match status.code() {
        Some(0) => Outcome::Done,
        Some(126) => Outcome::Cancelled,
        Some(127) if errors.contains("No authentication agent") => Outcome::NoPrompt(by_hand),
        Some(127) => Outcome::Refused,
        _ => Outcome::Failed(errors.lines().last().unwrap_or_default().trim_start_matches("anythingd: ").to_owned()),
    }
}
