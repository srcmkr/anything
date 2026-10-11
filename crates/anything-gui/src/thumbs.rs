//! Thumbnails, following the freedesktop.org thumbnail layout inside the
//! folder chosen in the settings: `<dir>/large/<md5 of file URI>.png`, and
//! `<dir>/fail/anything/…` for files that could not be thumbnailed.
//!
//! Only that folder is read and written. Thumbnails are made on request,
//! for the tiles on screen, newest request first: common images by
//! Anything itself, everything else by the system's thumbnailers. Nothing
//! is made in advance.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, SystemTime},
};

use gpui::{AppContext as _, Context, Global};
use md5::{Digest, Md5};

use crate::{demo, desktop::file_uri, settings, thumbnailers::Registry};

/// Edge length of "large" thumbnails in the freedesktop spec.
const SIZE: u32 = 256;
/// Thumbnails generated at the same time.
const PARALLEL: usize = 4;
/// Older requests beyond this are dropped; they come back if shown again.
const MAX_QUEUE: usize = 256;
/// Larger files get no preview, except videos (thumbnailers read only a bit).
const MAX_BYTES: u64 = 200 * 1024 * 1024;
/// Thumbnails not looked at for this long are removed.
const KEEP_UNUSED: Duration = Duration::from_secs(90 * 24 * 3600);
/// Subfolder of `fail/` for this program, as the spec asks.
const FAIL_DIR: &str = "anything";

/// Formats Anything decodes itself.
const OWN_DECODER: &[&str] = &["image/jpeg", "image/png", "image/gif", "image/webp", "image/bmp", "image/tiff"];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Folder,
    Image,
    Video,
    Audio,
    Document,
    Spreadsheet,
    Presentation,
    Archive,
    Code,
    Other,
}

/// The icon family for a file, by extension.
pub fn kind(name: &str, is_dir: bool) -> Kind {
    if is_dir {
        return Kind::Folder;
    }
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "tif" | "tiff" | "svg" | "heic" | "jxl" => Kind::Image,
        "mp4" | "mkv" | "webm" | "mov" | "avi" | "m4v" => Kind::Video,
        "mp3" | "flac" | "ogg" | "m4a" | "wav" | "opus" => Kind::Audio,
        "pdf" | "doc" | "docx" | "odt" | "txt" | "md" | "rtf" | "epub" => Kind::Document,
        "xls" | "xlsx" | "ods" | "csv" => Kind::Spreadsheet,
        "ppt" | "pptx" | "odp" => Kind::Presentation,
        "zip" | "gz" | "tgz" | "xz" | "zst" | "7z" | "rar" | "tar" => Kind::Archive,
        "rs" | "ts" | "tsx" | "js" | "py" | "go" | "c" | "h" | "json" | "toml" | "yaml" | "css"
        | "html" => Kind::Code,
        _ => Kind::Other,
    }
}

enum State {
    Queued,
    Running,
    Ready(Arc<Path>),
    Failed,
}

struct Job {
    path: PathBuf,
    mime: String,
    thumb: PathBuf,
    fail: PathBuf,
}

/// Bumped by "Clear preview cache" in the settings.
pub struct CacheCleared(pub u64);

impl Global for CacheCleared {}

pub struct Thumbnails {
    dir: PathBuf,
    enabled: bool,
    registry: Arc<Registry>,
    states: HashMap<PathBuf, State>,
    /// Newest last.
    queue: Vec<Job>,
    running: usize,
}

impl Thumbnails {
    pub fn new(cx: &mut Context<Self>) -> Self {
        cx.observe_global::<settings::AppSettings>(|this, cx| {
            let settings = settings::get(cx);
            if settings.thumbnail_dir != this.dir || settings.previews != this.enabled {
                this.reset(settings.thumbnail_dir.clone(), settings.previews);
                cx.notify();
            }
        })
        .detach();
        cx.observe_global::<CacheCleared>(|this, cx| {
            this.reset(this.dir.clone(), this.enabled);
            cx.notify();
        })
        .detach();
        let settings = settings::get(cx);
        let this = Self {
            dir: settings.thumbnail_dir.clone(),
            enabled: settings.previews,
            registry: Arc::new(Registry::load()),
            states: HashMap::new(),
            queue: Vec::new(),
            running: 0,
        };
        let dir = this.dir.clone();
        cx.background_spawn(async move { clean_up(&dir) }).detach();
        this
    }

    fn reset(&mut self, dir: PathBuf, enabled: bool) {
        self.dir = dir;
        self.enabled = enabled;
        self.states.clear();
        self.queue.clear();
    }

    /// The thumbnail for `path` if there is one; otherwise starts making it
    /// (if it can be made) and notifies when done.
    pub fn get(&mut self, path: &Path, size: u64, mtime: i64, cx: &mut Context<Self>) -> Option<Arc<Path>> {
        if !self.enabled {
            return None;
        }
        match self.states.get(path) {
            Some(State::Ready(thumb)) => return Some(thumb.clone()),
            Some(State::Failed | State::Running) => return None,
            Some(State::Queued) => {
                // Seen again: move to the front of the line.
                if let Some(at) = self.queue.iter().position(|job| job.path == path) {
                    let job = self.queue.remove(at);
                    self.queue.push(job);
                }
                return None;
            }
            None => {}
        }
        let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
        let mime = self.registry.mime_of(&name).map(str::to_owned);
        let usable = mime.as_deref().is_some_and(|mime| {
            (OWN_DECODER.contains(&mime) || self.registry.can_thumbnail(mime))
                && (size <= MAX_BYTES || mime.starts_with("video/"))
        });
        if !usable || !demo::may_preview(path) {
            self.states.insert(path.to_owned(), State::Failed);
            return None;
        }
        let hash = uri_hash(path);
        let thumb = self.dir.join("large").join(format!("{hash}.png"));
        let fail = self.dir.join("fail").join(FAIL_DIR).join(format!("{hash}.png"));
        if is_fresh(&thumb, mtime) {
            let thumb: Arc<Path> = thumb.into();
            self.states.insert(path.to_owned(), State::Ready(thumb.clone()));
            return Some(thumb);
        }
        if is_fresh(&fail, mtime) {
            self.states.insert(path.to_owned(), State::Failed);
            return None;
        }
        self.states.insert(path.to_owned(), State::Queued);
        self.queue.push(Job { path: path.to_owned(), mime: mime.unwrap_or_default(), thumb, fail });
        if self.queue.len() > MAX_QUEUE {
            let dropped = self.queue.remove(0);
            self.states.remove(&dropped.path);
        }
        self.pump(cx);
        None
    }

    fn pump(&mut self, cx: &mut Context<Self>) {
        while self.running < PARALLEL {
            let Some(job) = self.queue.pop() else { return };
            self.running += 1;
            self.states.insert(job.path.clone(), State::Running);
            let registry = self.registry.clone();
            cx.spawn(async move |this, cx| {
                let (path, thumb, made) = cx
                    .background_spawn(async move {
                        let made = make(&registry, &job);
                        if made.is_err() {
                            // Remember the failure so it is not tried again
                            // until the file changes.
                            if let Some(folder) = job.fail.parent() {
                                std::fs::create_dir_all(folder).ok();
                            }
                            std::fs::write(&job.fail, b"").ok();
                        }
                        (job.path, job.thumb, made)
                    })
                    .await;
                this.update(cx, |this, cx| {
                    this.running -= 1;
                    let state = if made.is_ok() { State::Ready(thumb.into()) } else { State::Failed };
                    this.states.insert(path, state);
                    this.pump(cx);
                    cx.notify();
                })
                .ok();
            })
            .detach();
        }
    }
}

fn modified_secs(path: &Path) -> Option<i64> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64)
}

fn is_fresh(thumb: &Path, mtime: i64) -> bool {
    modified_secs(thumb).is_some_and(|t| t >= mtime)
}

/// The `file://` URI, percent-encoded as the freedesktop spec expects.
/// MD5 of the file URI, as the freedesktop spec names thumbnails.
fn uri_hash(path: &Path) -> String {
    Md5::digest(file_uri(path).as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn make(registry: &Registry, job: &Job) -> Result<(), String> {
    let folder = job.thumb.parent().ok_or("thumbnail path has no folder")?;
    std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
    // Write next to the target and rename, so readers never see half a file.
    let tmp = job.thumb.with_extension(format!("{}.tmp.png", std::process::id()));
    let result = if OWN_DECODER.contains(&job.mime.as_str()) {
        image::ImageReader::open(&job.path)
            .and_then(|reader| reader.with_guessed_format())
            .map_err(|e| e.to_string())
            .and_then(|reader| reader.decode().map_err(|e| e.to_string()))
            .and_then(|image| image.thumbnail(SIZE, SIZE).save(&tmp).map_err(|e| e.to_string()))
    } else {
        registry.run(&job.mime, &job.path, &tmp, SIZE, file_uri)
    };
    match result.and_then(|()| {
        if tmp.is_file() { Ok(()) } else { Err("thumbnailer wrote nothing".into()) }
    }) {
        Ok(()) => std::fs::rename(&tmp, &job.thumb).map_err(|e| e.to_string()),
        Err(err) => {
            std::fs::remove_file(&tmp).ok();
            Err(err)
        }
    }
}

/// Removes thumbnails not looked at for [`KEEP_UNUSED`], at most once a day.
fn clean_up(dir: &Path) {
    let marker = dir.join(".anything-cleanup");
    let now = SystemTime::now();
    let recent = std::fs::metadata(&marker)
        .and_then(|m| m.modified())
        .is_ok_and(|t| now.duration_since(t).unwrap_or_default() < Duration::from_secs(24 * 3600));
    if recent || !dir.is_dir() {
        return;
    }
    for sub in [dir.join("large"), dir.join("fail").join(FAIL_DIR)] {
        let Ok(entries) = std::fs::read_dir(&sub) else { continue };
        for entry in entries.flatten() {
            let unused = entry
                .metadata()
                .and_then(|m| m.accessed())
                .is_ok_and(|t| now.duration_since(t).unwrap_or_default() > KEEP_UNUSED);
            if unused {
                std::fs::remove_file(entry.path()).ok();
            }
        }
    }
    std::fs::write(marker, b"").ok();
}

/// Deletes every thumbnail Anything keeps in `dir`.
pub fn clear(dir: &Path) {
    for sub in [dir.join("large"), dir.join("fail").join(FAIL_DIR)] {
        std::fs::remove_dir_all(sub).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freedesktop_names() {
        // Reference value from the freedesktop thumbnail spec's example.
        assert_eq!(uri_hash(Path::new("/home/jens/photos/me.png")), "c6ee772d9e49320e97ec29a7eb5b1697");
        assert_eq!(file_uri(Path::new("/a b/ü.jpg")), "file:///a%20b/%C3%BC.jpg");
        assert_eq!(kind("Strand 001.JPG", false), Kind::Image);
        assert_eq!(kind("clip.mp4", false), Kind::Video);
        assert_eq!(kind("x", true), Kind::Folder);
    }

    #[test]
    fn fake_entries_are_never_previewed_in_demo_mode() {
        demo::set_demo_mode(true);
        assert!(demo::may_preview(&demo::demo_dir().join("Bilder/a.jpg")));
        assert!(!demo::may_preview(Path::new("/home/user/Bilder/a.jpg")));
        assert!(!demo::may_preview(Path::new("/usr/share/backgrounds/x.jpg")));
        demo::set_demo_mode(false);
        assert!(demo::may_preview(Path::new("/usr/share/backgrounds/x.jpg")));
    }
}
