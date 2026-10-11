//! How Anything is installed, and where its parts are then.

use std::path::{Path, PathBuf};

/// Where one installation keeps its programs and the service's unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paths {
    pub app: &'static str,
    pub cli: &'static str,
    pub service: &'static str,
    pub unit: &'static str,
}

/// The Debian package.
pub const DEB: Paths = Paths {
    app: "/usr/bin/anything",
    cli: "/usr/bin/any",
    service: "/usr/libexec/anything/anythingd",
    unit: "/usr/lib/systemd/system/anythingd.service",
};

/// Installed from the settings ("Install for all users", the service
/// buttons) or by an update of such an installation.
pub const LOCAL: Paths = Paths {
    app: "/usr/local/bin/anything",
    cli: "/usr/local/bin/any",
    service: "/usr/local/libexec/anything/anythingd",
    unit: "/etc/systemd/system/anythingd.service",
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Install {
    /// From the Debian package; apt owns the files.
    Deb,
    /// Under `/usr/local`.
    Local,
    /// An AppImage; its file is where it was put.
    AppImage(PathBuf),
    /// Run from where it was built.
    Build,
}

impl Install {
    /// How the running program is installed.
    pub fn current() -> Self {
        if let Some(image) = std::env::var_os("APPIMAGE").map(PathBuf::from).filter(|p| p.is_file()) {
            return Install::AppImage(image);
        }
        let exe = std::env::current_exe().and_then(|e| e.canonicalize()).unwrap_or_default();
        Self::of(&exe)
    }

    fn of(exe: &Path) -> Self {
        let is = |paths: &Paths| [paths.app, paths.cli, paths.service].iter().any(|p| exe == Path::new(p));
        if is(&DEB) {
            Install::Deb
        } else if is(&LOCAL) {
            Install::Local
        } else {
            Install::Build
        }
    }

    /// The app to start after an update.
    pub fn app(&self) -> Option<PathBuf> {
        match self {
            Install::Deb => Some(DEB.app.into()),
            Install::Local => Some(LOCAL.app.into()),
            Install::AppImage(image) => Some(image.clone()),
            Install::Build => None,
        }
    }
}

/// Where the service is installed: from the package if it is there.
pub fn service_paths() -> &'static Paths {
    if Path::new(DEB.service).exists() { &DEB } else { &LOCAL }
}

/// `@SERVICE@` and `@APP@` in a packaging template, for `paths`.
pub fn fill(template: &str, paths: &Paths) -> String {
    template.replace("@SERVICE@", paths.service).replace("@APP@", paths.app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knows_where_it_runs() {
        assert_eq!(Install::of(Path::new("/usr/bin/anything")), Install::Deb);
        assert_eq!(Install::of(Path::new("/usr/libexec/anything/anythingd")), Install::Deb);
        assert_eq!(Install::of(Path::new("/usr/local/bin/any")), Install::Local);
        assert_eq!(Install::of(Path::new("/home/me/anything/target/release/anything")), Install::Build);
        assert_eq!(fill("ExecStart=@SERVICE@\nExec=@APP@ --show", &DEB), "ExecStart=/usr/libexec/anything/anythingd\nExec=/usr/bin/anything --show");
    }
}
