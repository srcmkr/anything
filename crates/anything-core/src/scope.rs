//! Which folders a scan covers: a list of folders to include and a list of
//! patterns to exclude.
//!
//! - An include is a folder; everything below it is covered. `~` stands for
//!   the home folder.
//! - An exclude starting with `/` is a path whose segments may contain `*`
//!   for any run of characters within one folder name: `/home/*` excludes
//!   every folder directly in `/home`, with everything below.
//! - An exclude without `/` matches a folder or file name anywhere:
//!   `node_modules`, `.git`, `*.tmp`.

/// Folders programs keep their caches in: rebuilt by the programs, hardly
/// ever looked for, and often very busy. Left out with one switch.
pub const CACHES: &[&str] = &[
    // XDG: every program's cache folder.
    ".cache",
    // Chromium, Chrome and Electron apps (VS Code, Slack, Discord, …).
    "Cache",
    "Code Cache",
    "GPUCache",
    "DawnCache",
    "DawnGraphiteCache",
    "DawnWebGPUCache",
    "GraphiteDawnCache",
    "GrShaderCache",
    "ShaderCache",
    "CacheStorage",
    // Firefox.
    "cache2",
    "startupCache",
    // The system's package and program caches.
    "/var/cache",
];

/// JavaScript dependencies, often hundreds of thousands of files.
pub const NODE_MODULES: &str = "node_modules";

/// `own` excludes plus those the switches add.
pub fn excludes(own: &[String], caches: bool, node_modules: bool) -> Vec<String> {
    let mut all = own.to_vec();
    if caches {
        all.extend(CACHES.iter().map(|c| c.to_string()));
    }
    if node_modules {
        all.push(NODE_MODULES.to_owned());
    }
    all
}

/// One path segment pattern, `*` matching any run of characters.
fn segment_matches(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or_default();
    let Some(mut rest) = name.strip_prefix(first) else { return false };
    let mut parts: Vec<&str> = parts.collect();
    let last = parts.pop().unwrap_or_default();
    for part in parts {
        match rest.find(part) {
            Some(at) => rest = &rest[at + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

fn segments(path: &str) -> Vec<String> {
    path.split('/').filter(|s| !s.is_empty() && *s != ".").map(str::to_owned).collect()
}

fn expand_home(path: &str) -> String {
    match (path.strip_prefix('~'), std::env::var("HOME")) {
        (Some(rest), Ok(home)) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => path.to_owned(),
    }
}

enum Exclude {
    Path(Vec<String>),
    Name(String),
}

pub struct Scope {
    include: Vec<Vec<String>>,
    exclude: Vec<Exclude>,
}

impl Scope {
    pub fn new<I, E>(include: I, exclude: E) -> Self
    where
        I: IntoIterator,
        I::Item: AsRef<str>,
        E: IntoIterator,
        E::Item: AsRef<str>,
    {
        let include = include
            .into_iter()
            .map(|p| expand_home(p.as_ref().trim()))
            .filter(|p| p.starts_with('/'))
            .map(|p| segments(&p))
            .collect();
        let exclude = exclude
            .into_iter()
            .filter_map(|p| {
                let p = expand_home(p.as_ref().trim());
                if p.is_empty() {
                    None
                } else if p.starts_with('/') {
                    Some(Exclude::Path(segments(&p)))
                } else {
                    Some(Exclude::Name(p.trim_end_matches('/').to_owned()))
                }
            })
            .collect();
        Self { include, exclude }
    }

    /// Covers the whole file system with no exclusions.
    pub fn is_everything(&self) -> bool {
        self.exclude.is_empty() && self.include.iter().any(|i| i.is_empty())
    }

    /// Whether `path` (given as segments) lies in or below an included folder.
    pub fn is_included(&self, path: &[&str]) -> bool {
        self.include.iter().any(|inc| inc.len() <= path.len() && inc.iter().zip(path).all(|(a, b)| a == b))
    }

    /// Whether `path` lies on the way to an included folder, so a scan has to
    /// walk through it without listing it.
    pub fn leads_to_include(&self, path: &[&str]) -> bool {
        self.include.iter().any(|inc| inc.len() > path.len() && inc.iter().zip(path).all(|(a, b)| a == b))
    }

    /// Whether the entry at `path` itself matches an exclude. Entries below an
    /// excluded folder are the caller's business (a scan does not descend).
    pub fn is_excluded(&self, path: &[&str]) -> bool {
        let Some(name) = path.last() else { return false };
        self.exclude.iter().any(|ex| match ex {
            Exclude::Name(pattern) => segment_matches(pattern, name),
            Exclude::Path(pattern) => {
                pattern.len() == path.len()
                    && pattern.iter().zip(path).all(|(p, s)| segment_matches(p, s))
            }
        })
    }
}

impl Scope {
    /// Deepest level any include or path exclude reaches. Below it, only
    /// name patterns can still change whether something is covered.
    pub fn max_depth(&self) -> usize {
        let includes = self.include.iter().map(Vec::len);
        let paths = self.exclude.iter().filter_map(|e| match e {
            Exclude::Path(p) => Some(p.len()),
            Exclude::Name(_) => None,
        });
        includes.chain(paths).max().unwrap_or(0)
    }

    /// Whether `name` matches a name pattern (no `/`), wherever it is.
    pub fn excludes_name(&self, name: &str) -> bool {
        self.exclude.iter().any(|e| matches!(e, Exclude::Name(p) if segment_matches(p, name)))
    }

    pub fn has_name_patterns(&self) -> bool {
        self.exclude.iter().any(|e| matches!(e, Exclude::Name(_)))
    }

    /// [`Self::is_included`] for `parent` + `name`, without building the path.
    pub fn is_included_at(&self, parent: &[&str], name: &str) -> bool {
        let len = parent.len() + 1;
        self.include.iter().any(|inc| {
            inc.len() <= len && inc.iter().enumerate().all(|(i, s)| s == segment(parent, name, i))
        })
    }

    /// [`Self::leads_to_include`] for `parent` + `name`.
    pub fn leads_to_include_at(&self, parent: &[&str], name: &str) -> bool {
        let len = parent.len() + 1;
        self.include.iter().any(|inc| {
            inc.len() > len && (0..len).all(|i| inc[i] == segment(parent, name, i))
        })
    }

    /// [`Self::is_excluded`] for `parent` + `name`.
    pub fn is_excluded_at(&self, parent: &[&str], name: &str) -> bool {
        let len = parent.len() + 1;
        self.exclude.iter().any(|ex| match ex {
            Exclude::Name(pattern) => segment_matches(pattern, name),
            Exclude::Path(pattern) => {
                pattern.len() == len
                    && pattern.iter().enumerate().all(|(i, p)| segment_matches(p, segment(parent, name, i)))
            }
        })
    }
}

fn segment<'a>(parent: &[&'a str], name: &'a str, i: usize) -> &'a str {
    if i < parent.len() { parent[i] } else { name }
}

/// Identifies a set of scan settings, e.g. to tell whether a snapshot was
/// made with them.
pub fn tag(include: &[String], exclude: &[String]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for (list, items) in [(0u8, include), (1u8, exclude)] {
        for item in items {
            for byte in [list].iter().chain(item.as_bytes()).chain(&[0xff]) {
                hash = (hash ^ *byte as u64).wrapping_mul(0x0100_0000_01b3);
            }
        }
    }
    hash
}

/// Whether an exclude pattern lies outside every included folder, so it
/// cannot have any effect. Worth pointing out; it never blocks saving.
pub fn exclude_is_outside(include: &[String], pattern: &str) -> bool {
    let pattern = expand_home(pattern.trim());
    if !pattern.starts_with('/') {
        return false;
    }
    let scope = Scope::new(include, std::iter::empty::<&str>());
    let segs = segments(&pattern);
    // Compare the literal prefix up to the first wildcard segment.
    let literal: Vec<&str> = segs.iter().take_while(|s| !s.contains('*')).map(String::as_str).collect();
    let reachable = scope.is_included(&literal)
        || scope.leads_to_include(&literal)
        || scope.include.iter().any(|inc| inc.len() <= segs.len() && inc.iter().zip(&segs).all(|(a, b)| segment_matches(b, a)));
    !reachable
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(path: &str) -> Vec<&str> {
        path.split('/').filter(|s| !s.is_empty()).collect()
    }

    #[test]
    fn segment_patterns() {
        assert!(segment_matches("*", "anything"));
        assert!(segment_matches("*.tmp", "a.tmp"));
        assert!(!segment_matches("*.tmp", "a.tmp.bak"));
        assert!(segment_matches("node_modules", "node_modules"));
        assert!(!segment_matches("node_modules", "node_modules2"));
        assert!(segment_matches("a*b*c", "aXbYc"));
    }

    #[test]
    fn only_home() {
        let scope = Scope::new(["/home/user"], std::iter::empty::<&str>());
        assert!(scope.is_included(&p("/home/user")));
        assert!(scope.is_included(&p("/home/user/Dokumente/a.pdf")));
        assert!(!scope.is_included(&p("/home/other")));
        assert!(!scope.is_included(&p("/home/username")));
        assert!(scope.leads_to_include(&p("/home")));
        assert!(!scope.leads_to_include(&p("/usr")));
        assert!(!scope.is_everything());
    }

    #[test]
    fn everything_but_home_children() {
        let scope = Scope::new(["/"], ["/home/*"]);
        assert!(scope.is_included(&p("/usr/lib")));
        assert!(!scope.is_excluded(&p("/home")));
        assert!(scope.is_excluded(&p("/home/user")));
        // Below an excluded folder the scan never arrives; the entry itself
        // does not match the pattern.
        assert!(!scope.is_excluded(&p("/home/user/x")));
    }

    #[test]
    fn name_patterns_match_anywhere() {
        let scope = Scope::new(["/"], ["node_modules", ".git/"]);
        assert!(scope.is_excluded(&p("/srv/app/node_modules")));
        assert!(scope.is_excluded(&p("/home/u/src/.git")));
        assert!(!scope.is_excluded(&p("/home/u/src/git")));
        assert!(Scope::new(["/"], std::iter::empty::<&str>()).is_everything());
    }

    #[test]
    fn split_checks_agree() {
        let scope = Scope::new(["/home/user", "/srv"], ["/home/*/.cache", "node_modules"]);
        for path in ["/home", "/home/user", "/home/user/.cache", "/home/other", "/srv/x/node_modules", "/usr"] {
            let segs = p(path);
            let (name, parent) = segs.split_last().unwrap();
            assert_eq!(scope.is_included(&segs), scope.is_included_at(parent, name), "{path}");
            assert_eq!(scope.leads_to_include(&segs), scope.leads_to_include_at(parent, name), "{path}");
            assert_eq!(scope.is_excluded(&segs), scope.is_excluded_at(parent, name), "{path}");
        }
    }

    #[test]
    fn hints() {
        let include = vec!["/home".to_owned()];
        assert!(!exclude_is_outside(&include, "/home/*/.cache"));
        assert!(!exclude_is_outside(&include, "/*/user"));
        assert!(exclude_is_outside(&include, "/usr/lib"));
        assert!(!exclude_is_outside(&include, "node_modules"));
    }
}
