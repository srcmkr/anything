//! The system's thumbnailers, as file managers use them: MIME types from the
//! shared MIME database (`mime/globs2`) and one command per type from
//! `thumbnailers/*.thumbnailer`, both looked up in the XDG data folders.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::OnceLock,
    time::{Duration, Instant},
};

/// Longest a thumbnailer may take before it is stopped.
const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct Registry {
    /// Lower-case extension (without the dot) to MIME type.
    by_extension: HashMap<String, String>,
    /// MIME type to command template (`%i`, `%u`, `%o`, `%s`).
    commands: HashMap<String, Vec<String>>,
}

fn data_dirs() -> Vec<PathBuf> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let system = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    // Earlier folders win, so the home folder goes first.
    home.into_iter().chain(system.split(':').filter(|d| !d.is_empty()).map(PathBuf::from)).collect()
}

fn on_path(program: &str) -> bool {
    if program.contains('/') {
        return Path::new(program).is_file();
    }
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(program).is_file()))
}

/// Splits an `Exec=` line into words, honouring double quotes.
fn split_exec(exec: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    for c in exec.chars() {
        match c {
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

impl Registry {
    pub fn load() -> Self {
        let mut registry = Registry::default();
        for dir in data_dirs().iter().rev() {
            // Later (more important) folders overwrite earlier ones.
            if let Ok(globs) = std::fs::read_to_string(dir.join("mime/globs2")) {
                registry.read_globs(&globs);
            }
            let Ok(entries) = std::fs::read_dir(dir.join("thumbnailers")) else { continue };
            for entry in entries.flatten() {
                if entry.path().extension().is_some_and(|e| e == "thumbnailer")
                    && let Ok(text) = std::fs::read_to_string(entry.path())
                {
                    registry.read_thumbnailer(&text);
                }
            }
        }
        registry
    }

    fn read_globs(&mut self, text: &str) {
        // weight:type:glob[:flags], highest weight first in the file.
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let mut fields = line.split(':');
            let (Some(_), Some(mime), Some(glob)) = (fields.next(), fields.next(), fields.next()) else { continue };
            let Some(ext) = glob.strip_prefix("*.") else { continue };
            if ext.contains(['*', '?', '[']) {
                continue;
            }
            self.by_extension.entry(ext.to_lowercase()).or_insert_with(|| mime.to_owned());
        }
    }

    fn read_thumbnailer(&mut self, text: &str) {
        let field = |key: &str| text.lines().find_map(|l| l.strip_prefix(key)).map(str::trim);
        let (Some(exec), Some(types)) = (field("Exec="), field("MimeType=")) else { return };
        let try_exec = field("TryExec=");
        let command = split_exec(exec);
        let Some(program) = command.first() else { return };
        if !on_path(try_exec.unwrap_or(program)) {
            return;
        }
        for mime in types.split(';').filter(|m| !m.is_empty()) {
            self.commands.insert(mime.to_owned(), command.clone());
        }
    }

    /// MIME type by file name; the longest known extension wins (`.tar.gz`).
    pub fn mime_of(&self, name: &str) -> Option<&str> {
        let lower = name.to_lowercase();
        lower
            .match_indices('.')
            .find_map(|(at, _)| self.by_extension.get(&lower[at + 1..]))
            .map(String::as_str)
    }

    pub fn can_thumbnail(&self, mime: &str) -> bool {
        self.commands.contains_key(mime)
    }

    /// Runs the thumbnailer for `mime` on `input`, writing a PNG to `output`.
    ///
    /// Thumbnailers take apart whatever a file contains, so a crafted file
    /// can attack them. Where bubblewrap works they run locked away: no
    /// network, no home folder, nothing of the user's but the one file.
    pub fn run(&self, mime: &str, input: &Path, output: &Path, size: u32, uri_of: fn(&Path) -> String) -> Result<(), String> {
        let template = self.commands.get(mime).ok_or("no thumbnailer")?;
        let fill = |input: &Path, output: &Path| -> Vec<String> {
            template
                .iter()
                .map(|word| {
                    word.replace("%i", &input.to_string_lossy())
                        .replace("%u", &uri_of(input))
                        .replace("%o", &output.to_string_lossy())
                        .replace("%s", &size.to_string())
                        .replace("%%", "%")
                })
                .collect()
        };
        let Some(sandbox) = Sandbox::get() else {
            let args = fill(input, output);
            let mut command = Command::new("nice");
            command.args(["-n", "10"]).args(&args);
            return wait(command, &args[0]);
        };
        // The file appears inside as /in/file.<ext>; the thumbnailer writes
        // into an empty folder that becomes /out.
        let inner_input = Path::new("/in").join(format!("file{}", safe_extension(input)));
        let inner_output = Path::new("/out/thumbnail.png");
        let args = fill(&inner_input, inner_output);
        let work = output.with_extension("sandbox");
        std::fs::create_dir_all(&work).map_err(|e| e.to_string())?;
        let mut command = Command::new("nice");
        command.args(["-n", "10", "bwrap"]).args(&sandbox.args);
        if let Some(program) = find_program(&args[0]).filter(|p| !p.starts_with("/usr/")) {
            command.arg("--ro-bind").arg(&program).arg(&program);
        }
        command.arg("--ro-bind").arg(input).arg(&inner_input);
        command.arg("--bind").arg(&work).arg("/out");
        command.arg("--").args(&args);
        let result = wait(command, &args[0]).and_then(|()| {
            std::fs::rename(work.join("thumbnail.png"), output).map_err(|_| "thumbnailer wrote nothing".to_string())
        });
        std::fs::remove_dir_all(&work).ok();
        result
    }
}

/// Starts `command` quietly and stops it after [`TIMEOUT`].
fn wait(mut command: Command, name: &str) -> Result<(), String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    let start = Instant::now();
    loop {
        match child.try_wait().map_err(|e| e.to_string())? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(format!("{name} exited with {status}")),
            None if start.elapsed() > TIMEOUT => {
                child.kill().ok();
                child.wait().ok();
                return Err(format!("{name} took too long"));
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}

/// `.pdf` and the like, for thumbnailers that go by the name; anything
/// unusual is left out.
fn safe_extension(path: &Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.len() <= 16 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(|e| format!(".{e}"))
        .unwrap_or_default()
}

fn find_program(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        return Some(PathBuf::from(program));
    }
    std::env::split_paths(&std::env::var_os("PATH")?).map(|dir| dir.join(program)).find(|p| p.is_file())
}

/// The bubblewrap arguments every thumbnailer runs with.
struct Sandbox {
    args: Vec<String>,
}

impl Sandbox {
    /// `None` when bubblewrap is missing or cannot set up a sandbox here;
    /// thumbnailers then run as before, as the user.
    fn get() -> Option<&'static Sandbox> {
        static SANDBOX: OnceLock<Option<Sandbox>> = OnceLock::new();
        SANDBOX
            .get_or_init(|| {
                let sandbox = Sandbox::new();
                let works = Command::new("bwrap")
                    .args(&sandbox.args)
                    .args(["--", "true"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .is_ok_and(|s| s.success());
                if !works {
                    eprintln!("anything: thumbnailers run without a sandbox (bubblewrap missing or not allowed)");
                }
                works.then_some(sandbox)
            })
            .as_ref()
    }

    fn new() -> Self {
        let mut args: Vec<String> = Vec::new();
        let mut add = |words: &[&str]| args.extend(words.iter().map(|w| w.to_string()));
        // Programs, libraries and system settings, read-only. The home
        // folder, /run (with its sockets) and other users' files stay out.
        add(&["--ro-bind", "/usr", "/usr", "--ro-bind", "/etc", "/etc"]);
        for top in ["/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32"] {
            match std::fs::read_link(top) {
                Ok(target) => add(&["--symlink", &target.to_string_lossy(), top]),
                Err(_) if Path::new(top).is_dir() => add(&["--ro-bind", top, top]),
                Err(_) => {}
            }
        }
        add(&["--ro-bind-try", "/var/cache/fontconfig", "/var/cache/fontconfig"]);
        add(&["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--dir", "/in"]);
        // No network, no view of other processes; gone when Anything goes.
        add(&["--unshare-all", "--die-with-parent", "--new-session", "--chdir", "/tmp"]);
        add(&["--clearenv", "--setenv", "PATH", "/usr/local/bin:/usr/bin:/bin", "--setenv", "HOME", "/tmp"]);
        add(&["--setenv", "GIO_USE_VFS", "local"]);
        if let Ok(lang) = std::env::var("LANG") {
            add(&["--setenv", "LANG", &lang]);
        }
        Sandbox { args }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_globs_and_thumbnailers() {
        let mut r = Registry::default();
        r.read_globs("# comment\n50:application/pdf:*.pdf\n50:application/x-compressed-tar:*.tar.gz\n50:application/gzip:*.gz\n10:text/x-readme:README*\n");
        r.read_thumbnailer("[Thumbnailer Entry]\nTryExec=sh\nExec=sh -c \"echo %i\" %o\nMimeType=application/pdf;\n");
        r.read_thumbnailer("[Thumbnailer Entry]\nTryExec=no-such-program-here\nExec=x %i\nMimeType=application/gzip;\n");
        assert_eq!(r.mime_of("Rechnung.PDF"), Some("application/pdf"));
        assert_eq!(r.mime_of("backup.tar.gz"), Some("application/x-compressed-tar"));
        assert_eq!(r.mime_of("README"), None);
        assert!(r.can_thumbnail("application/pdf"));
        // A thumbnailer whose program is missing is ignored.
        assert!(!r.can_thumbnail("application/gzip"));
        assert_eq!(r.commands["application/pdf"], ["sh", "-c", "echo %i", "%o"]);
    }

    #[test]
    fn sandbox_shows_only_the_one_file() {
        if Sandbox::get().is_none() {
            eprintln!("bubblewrap not usable here; skipped");
            return;
        }
        let dir = std::env::temp_dir().join(format!("anything-sandbox-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("crafted \"name\"; rm -rf ~.pdf");
        std::fs::write(&input, b"content").unwrap();
        let secret = dir.join("other-file");
        std::fs::write(&secret, b"x").unwrap();
        let home = std::env::var("HOME").unwrap();
        let check = format!(
            "test ! -e '{home}' && test ! -e '{}' && test ! -e '{}' && test \"$1\" = /in/file.pdf && cp \"$1\" \"$2\"",
            secret.display(),
            input.display()
        );
        let mut r = Registry::default();
        r.commands.insert("test/x".into(), vec!["sh".into(), "-c".into(), check, "sh".into(), "%i".into(), "%o".into()]);
        let output = dir.join("out.png");
        r.run("test/x", &input, &output, 256, |p| format!("file://{}", p.display())).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"content");
        // Nothing left behind but the result.
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }
}
