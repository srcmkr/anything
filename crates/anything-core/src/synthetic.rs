//! Deterministic fake file tree for development, benchmarks and screenshots.
//!
//! Names follow the patterns people actually search for: office documents
//! with numbers and dates, camera and screenshot files, source trees.

use std::collections::HashMap;

use crate::{Index, IndexBuilder, NO_PARENT};

/// A real file or folder placed into the fake tree under its own path, so it
/// can be opened and previewed.
#[derive(Clone, Debug)]
pub struct RealEntry {
    /// Absolute path.
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub mtime: i64,
}

/// Folders created by path, so real entries and fake trees share them.
struct Folders {
    ids: HashMap<String, u32>,
}

impl Folders {
    fn get(&mut self, b: &mut IndexBuilder, path: &str, mtime: i64) -> u32 {
        let path = path.trim_end_matches('/');
        if let Some(&id) = self.ids.get(path) {
            return id;
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        let parent = self.get(b, parent, mtime);
        let id = b.push(parent, name, true, 0, mtime);
        self.ids.insert(path.to_owned(), id);
        id
    }
}

const DOC_WORDS: &[&str] = &[
    "Rechnung", "Sammelrechnung", "Rechnungen", "Angebot", "Auftrag", "Auftragsbestätigung",
    "Lieferschein", "Gutschrift", "Mahnung", "Vertrag", "Kündigung", "Kontoauszug",
    "Steuererklärung", "Lohnabrechnung", "Bewerbung", "Lebenslauf", "Zeugnis", "Protokoll",
    "Präsentation", "Bericht", "Notizen", "Übersicht", "Planung", "Entwurf", "Angebotsanfrage",
    "Invoice", "Report", "Summary", "Meeting-Notes", "Budget", "Quote", "Contract", "Résumé",
];

const PHOTO_WORDS: &[&str] = &[
    "Urlaub", "Hochzeit", "Geburtstag", "Familie", "Garten", "Weihnachten", "Ostsee",
    "Berlin", "Wanderung", "Party", "Hund", "Katze", "Sonnenuntergang", "Paris", "Tokyo",
];

const DOC_EXTENSIONS: &[&str] = &[
    "pdf", "pdf", "pdf", "docx", "odt", "xlsx", "ods", "pptx", "txt", "md", "csv",
];

const CODE_WORDS: &[&str] = &[
    "main", "lib", "mod", "config", "utils", "index", "app", "server", "client", "parser",
    "build", "test", "helpers", "types", "api", "routes", "model", "view", "theme", "README",
];

const CODE_EXTENSIONS: &[&str] =
    &["rs", "ts", "tsx", "js", "py", "go", "c", "h", "json", "toml", "yaml", "css", "html"];

const MUSIC_EXTENSIONS: &[&str] = &["mp3", "flac", "ogg", "m4a"];

const DIR_WORDS: &[&str] = &[
    "Dokumente", "Bilder", "Musik", "Videos", "Downloads", "Projekte", "Archiv", "Steuer",
    "Firma", "Privat", "Kunden", "Belege", "Scans", "Fotos", "Urlaub", "Backup", "Export",
    "src", "build", "assets", "docs", "tests", "node_modules", ".cache", "Übersicht",
    "Données", "Москва", "日本語",
];

/// Top-level folders with their share of all entries, roughly like a
/// desktop machine: most files live in the home folder.
const TOP_LEVEL: &[(&str, usize)] = &[
    ("home/user", 45),
    ("usr", 25),
    ("var", 8),
    ("opt", 8),
    ("srv", 8),
    ("etc", 3),
    ("bin", 3),
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }

    fn date(&mut self) -> String {
        format!("{}-{:02}-{:02}", 2015 + self.below(12), 1 + self.below(12), 1 + self.below(28))
    }
}

/// Appends a random file name to `out` and returns a plausible size.
fn file_name(rng: &mut Rng, out: &mut String) -> u64 {
    use std::fmt::Write;
    match rng.below(20) {
        // Office documents: Rechnung-123.pdf, 2024-03-01_Angebot.pdf, ...
        0..=6 => {
            let word = rng.pick(DOC_WORDS);
            match rng.below(5) {
                0 => write!(out, "{word}-{}", rng.below(5000)),
                1 => write!(out, "{}-{word}", rng.below(5000)),
                2 => write!(out, "{}_{word}", rng.date()),
                3 => write!(out, "{word} {}", rng.date()),
                _ => write!(out, "{word}"),
            }
            .unwrap();
            write!(out, ".{}", rng.pick(DOC_EXTENSIONS)).unwrap();
            10_000 + rng.below(5_000_000) as u64
        }
        // Camera, phone and screenshot files.
        7..=10 => {
            match rng.below(5) {
                0 => write!(out, "IMG_{:04}.jpg", rng.below(10_000)),
                1 => write!(out, "DSC{:05}.JPG", rng.below(100_000)),
                2 => write!(out, "{} {}.jpg", rng.pick(PHOTO_WORDS), rng.below(400)),
                3 => write!(out, "Screenshot {} {:02}-{:02}.png", rng.date(), rng.below(24), rng.below(60)),
                _ => write!(out, "PXL_{}_{:06}.jpg", rng.date().replace('-', ""), rng.below(1_000_000)),
            }
            .unwrap();
            200_000 + rng.below(12_000_000) as u64
        }
        11 => {
            match rng.below(2) {
                0 => write!(out, "VID_{}_{:06}.mp4", rng.date().replace('-', ""), rng.below(1_000_000)),
                _ => write!(out, "{} {}.mkv", rng.pick(PHOTO_WORDS), rng.below(50)),
            }
            .unwrap();
            20_000_000 + rng.below(2_000_000_000) as u64
        }
        12 => {
            write!(out, "{:02} - Track {}.{}", 1 + rng.below(20), rng.below(100), rng.pick(MUSIC_EXTENSIONS))
                .unwrap();
            2_000_000 + rng.below(30_000_000) as u64
        }
        // Source trees.
        _ => {
            write!(out, "{}", rng.pick(CODE_WORDS)).unwrap();
            if rng.below(3) == 0 {
                write!(out, "_{}", rng.pick(CODE_WORDS)).unwrap();
            }
            write!(out, ".{}", rng.pick(CODE_EXTENSIONS)).unwrap();
            100 + rng.below(80_000) as u64
        }
    }
}

/// Builds an index with `entries` entries that look roughly like a real disk.
pub fn synthetic_index(entries: usize, seed: u64) -> Index {
    synthetic_index_with(entries, seed, &[])
}

/// Like [`synthetic_index`], with `real` entries placed under their own paths.
pub fn synthetic_index_with(entries: usize, seed: u64, real: &[RealEntry]) -> Index {
    let mut rng = Rng(seed | 1);
    let mut b = IndexBuilder::with_capacity(entries + real.len());
    let mut dirs: Vec<u32> = Vec::new();

    let root = b.push(NO_PARENT, "", true, 0, 1_500_000_000);
    let mut folders = Folders { ids: HashMap::from([(String::new(), root)]) };
    let mut real: Vec<&RealEntry> = real.iter().collect();
    real.sort_by(|a, c| a.path.cmp(&c.path));
    for entry in real {
        if entry.is_dir {
            folders.get(&mut b, &entry.path, entry.mtime);
        } else if let Some((parent, name)) = entry.path.rsplit_once('/') {
            let parent = folders.get(&mut b, parent, entry.mtime);
            b.push(parent, name, false, entry.size, entry.mtime);
        }
    }

    let mut name = String::new();
    let mut created = 1;
    for (path, share) in TOP_LEVEL {
        // Each top-level tree grows on its own until it has its share.
        let parent = folders.get(&mut b, &format!("/{path}"), 1_500_000_000);
        dirs.clear();
        dirs.push(parent);
        created += share;
        let target = (entries * created / 100).min(entries);
        grow(&mut b, &mut rng, &mut dirs, &mut name, target);
    }
    grow(&mut b, &mut rng, &mut dirs, &mut name, entries);
    b.build()
}

fn grow(b: &mut IndexBuilder, rng: &mut Rng, dirs: &mut Vec<u32>, name: &mut String, target: usize) {
    while b.len() < target {
        // Prefer recently created directories so the tree gets deep.
        let span = dirs.len().min(64);
        let parent = if rng.below(4) == 0 {
            dirs[rng.below(dirs.len())]
        } else {
            dirs[dirs.len() - 1 - rng.below(span)]
        };
        let mtime = 1_420_000_000 + rng.below(370_000_000) as i64;

        name.clear();
        if rng.below(10) == 0 {
            name.push_str(rng.pick(DIR_WORDS));
            if rng.below(2) == 0 {
                name.push_str(&format!(" {}", 2015 + rng.below(12)));
            }
            dirs.push(b.push(parent, name, true, 0, mtime));
        } else {
            let size = file_name(rng, name);
            b.push(parent, name, false, size, mtime);
        }
    }
}
