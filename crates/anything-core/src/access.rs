//! Who may see which entries, and what the scan settings cover.
//!
//! An entry is visible to a user when the user could list it with `ls`:
//! read permission on its folder, and search (execute) permission on every
//! folder above that. Folders with a POSIX ACL are judged conservatively:
//! anyone but the owner only gets what the mode bits allow "others", so an
//! entry may stay hidden that the ACL would show, but never the other way
//! round. Root sees everything.

use std::borrow::Cow;

use rayon::prelude::*;

use crate::{FLAG_DEAD, FLAG_DIR, Index, NO_PARENT, Owner, Scope};

const READ: u16 = 4;
const SEARCH: u16 = 1;

/// The user asking: uid and every group the process is in.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Viewer {
    pub uid: u32,
    pub groups: Vec<u32>,
}

impl Viewer {
    fn may(&self, owner: Option<Owner>, bit: u16) -> bool {
        if self.uid == 0 {
            return true;
        }
        // Unknown ownership: hide rather than guess.
        let Some(o) = owner else { return false };
        let bits = if o.uid == self.uid {
            o.mode >> 6
        } else if !o.acl && self.groups.contains(&o.gid) {
            o.mode >> 3
        } else {
            o.mode
        };
        bits & bit != 0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Cover {
    /// On the way to an included folder, not included itself.
    Outside,
    Inside,
    Excluded,
}

/// What `scope` covers, per entry. Entries must come after their parents.
///
/// Paths only matter down to [`Scope::max_depth`]; below that an entry is
/// covered like its folder, unless a name pattern says otherwise, and name
/// patterns are checked once per distinct name.
pub(crate) fn coverage(index: &Index, scope: &Scope) -> Vec<Cover> {
    let n = index.store.entries.len();
    let max_depth = scope.max_depth();
    let excluded_names: Option<Vec<bool>> = scope.has_name_patterns().then(|| {
        (0..index.names.len() as u32).into_par_iter().map(|name| scope.excludes_name(&index.names.text(name))).collect()
    });
    let mut cover = vec![Cover::Outside; n];
    let mut depth = vec![0u16; n];
    // Path segments of shallow folders only.
    let mut segments: std::collections::HashMap<u32, Vec<Cow<'_, str>>> = std::collections::HashMap::new();
    for id in 0..n {
        let e = &index.store.entries[id];
        if e.parent == NO_PARENT {
            cover[id] = if scope.is_included(&[]) { Cover::Inside } else { Cover::Outside };
            segments.insert(id as u32, Vec::new());
            continue;
        }
        let parent = e.parent as usize;
        if cover[parent] == Cover::Excluded {
            cover[id] = Cover::Excluded;
            continue;
        }
        depth[id] = depth[parent].saturating_add(1);
        let by_name = excluded_names.as_ref().is_some_and(|x| x[e.name as usize]);
        if depth[id] as usize > max_depth {
            cover[id] = if cover[parent] == Cover::Inside && !by_name { Cover::Inside } else { Cover::Excluded };
            continue;
        }
        let Some(above) = segments.get(&(parent as u32)) else {
            cover[id] = Cover::Excluded;
            continue;
        };
        let name = index.names.text(e.name);
        let refs: Vec<&str> = above.iter().map(|s| s.as_ref()).collect();
        let is_dir = e.flags & FLAG_DIR != 0;
        cover[id] = if by_name || scope.is_excluded_at(&refs, &name) {
            Cover::Excluded
        } else if cover[parent] == Cover::Inside || scope.is_included_at(&refs, &name) {
            Cover::Inside
        } else if is_dir && scope.leads_to_include_at(&refs, &name) {
            Cover::Outside
        } else {
            Cover::Excluded
        };
        if is_dir && cover[id] != Cover::Excluded && (depth[id] as usize) < max_depth {
            let mut own = above.clone();
            own.push(name);
            segments.insert(id as u32, own);
        }
    }
    cover
}

/// One bit per entry: set if `viewer` may see it (when given) and `scope`
/// covers it (when given). Entries must come after their parents, as every
/// index built here does.
pub fn mask(index: &Index, viewer: Option<&Viewer>, scope: Option<&Scope>) -> Vec<u64> {
    let n = index.store.entries.len();
    let cover = scope.map(|scope| coverage(index, scope));
    // Per folder: may the viewer list it? Passing through needs search
    // permission on every folder above.
    let mut list = vec![viewer.is_none(); n];
    if let Some(v) = viewer {
        let mut pass = vec![false; n];
        for id in 0..n {
            let e = &index.store.entries[id];
            if e.flags & FLAG_DIR == 0 {
                continue;
            }
            let reachable = e.parent == NO_PARENT || pass[e.parent as usize];
            let owner = index.owner(id as u32);
            pass[id] = reachable && v.may(owner, SEARCH);
            list[id] = reachable && v.may(owner, READ);
        }
    }
    let mut bits = vec![0u64; n.div_ceil(64)];
    bits.par_iter_mut().enumerate().for_each(|(word, out)| {
        for bit in 0..64 {
            let id = word * 64 + bit;
            if id >= n {
                break;
            }
            let e = &index.store.entries[id];
            let seen = e.parent != NO_PARENT && list[e.parent as usize];
            let covered = cover.as_ref().is_none_or(|c| c[id] == Cover::Inside);
            if e.parent != NO_PARENT && e.flags & FLAG_DEAD == 0 && seen && covered {
                *out |= 1 << bit;
            }
        }
    });
    bits
}

/// Extends a [`mask`] to entries appended since it was made (from `from`
/// on), checking each on its own by walking up to the root. For the few
/// entries a live update adds, that beats redoing the whole mask.
pub fn extend(index: &Index, viewer: Option<&Viewer>, scope: Option<&Scope>, bits: &mut Vec<u64>, from: usize) {
    let n = index.store.entries.len();
    bits.resize(n.div_ceil(64), 0);
    for id in from..n {
        if visible(index, viewer, scope, id as u32) {
            bits[id / 64] |= 1 << (id % 64);
        }
    }
}

fn visible(index: &Index, viewer: Option<&Viewer>, scope: Option<&Scope>, id: u32) -> bool {
    let e = &index.store.entries[id as usize];
    if e.flags & FLAG_DEAD != 0 || e.parent == NO_PARENT {
        return false;
    }
    // Folders from the root down to the parent.
    let mut chain = Vec::new();
    let mut cur = Some(e.parent);
    while let Some(c) = cur {
        chain.push(c);
        cur = index.parent(c);
    }
    chain.reverse();
    if let Some(v) = viewer {
        // Pass through every folder above the parent, and list the parent.
        let (parent, above) = chain.split_last().unwrap();
        if !above.iter().all(|&d| v.may(index.owner(d), SEARCH)) || !v.may(index.owner(*parent), READ) {
            return false;
        }
    }
    let Some(scope) = scope else { return true };
    let names: Vec<Cow<'_, str>> = chain.iter().skip(1).chain(std::iter::once(&id)).map(|&d| index.name(d)).collect();
    let mut segments: Vec<&str> = Vec::with_capacity(names.len());
    let mut inside = scope.is_included(&[]);
    for name in &names {
        if scope.is_excluded_at(&segments, name) {
            return false;
        }
        inside = inside || scope.is_included_at(&segments, name);
        segments.push(name);
    }
    inside
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{IndexBuilder, Query, SortKey};

    fn owned(uid: u32, gid: u32, mode: u16) -> Option<Owner> {
        Some(Owner { uid, gid, mode, acl: false })
    }

    /// /home (755 root)
    ///   alice (700 alice)   -> secret.txt
    ///   shared (750 root:staff) -> plan.txt
    ///   pub (755 alice)     -> hello.txt
    ///   acl (770 alice:staff, ACL) -> acl.txt
    fn sample() -> Index {
        let mut b = IndexBuilder::default();
        let root = b.push_folder_owned(NO_PARENT, "", 0, false, owned(0, 0, 0o755));
        let home = b.push_folder_owned(root, "home", 0, false, owned(0, 0, 0o755));
        let alice = b.push_folder_owned(home, "alice", 0, false, owned(1000, 1000, 0o700));
        b.push_file(alice, "secret.txt", 1, 0);
        let shared = b.push_folder_owned(home, "shared", 0, false, owned(0, 50, 0o750));
        b.push_file(shared, "plan.txt", 1, 0);
        let public = b.push_folder_owned(home, "pub", 0, false, owned(1000, 1000, 0o755));
        b.push_file(public, "hello.txt", 1, 0);
        let acl = b.push_folder_owned(home, "acl", 0, false, Some(Owner { uid: 1000, gid: 50, mode: 0o770, acl: true }));
        b.push_file(acl, "acl.txt", 1, 0);
        b.build()
    }

    fn visible(index: &Index, viewer: Option<&Viewer>, scope: Option<&Scope>) -> Vec<String> {
        let bits = mask(index, viewer, scope);
        let mut names: Vec<String> = index
            .search(&Query::parse(""), SortKey::Path, false)
            .into_iter()
            .filter(|&id| bits[id as usize / 64] & (1 << (id % 64)) != 0)
            .map(|id| index.path(id))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn users_see_what_they_could_list() {
        let index = sample();
        let alice = Viewer { uid: 1000, groups: vec![1000] };
        let bob_staff = Viewer { uid: 1001, groups: vec![1001, 50] };
        let eve = Viewer { uid: 1002, groups: vec![1002] };

        let all = visible(&index, Some(&Viewer { uid: 0, groups: vec![0] }), None);
        assert_eq!(all.len(), 9);
        assert_eq!(all, visible(&index, None, None));

        // Alice owns alice/, pub/ and acl/; shared/ is root:staff 750.
        assert_eq!(
            visible(&index, Some(&alice), None),
            ["/home", "/home/acl", "/home/acl/acl.txt", "/home/alice", "/home/alice/secret.txt", "/home/pub", "/home/pub/hello.txt", "/home/shared"]
        );
        // Bob is in staff: shared/ yes; acl/ has an ACL, so only "other"
        // bits count for him, and those are 0.
        assert_eq!(
            visible(&index, Some(&bob_staff), None),
            ["/home", "/home/acl", "/home/alice", "/home/pub", "/home/pub/hello.txt", "/home/shared", "/home/shared/plan.txt"]
        );
        // Eve sees the folder names in /home but nothing inside the closed ones.
        assert_eq!(
            visible(&index, Some(&eve), None),
            ["/home", "/home/acl", "/home/alice", "/home/pub", "/home/pub/hello.txt", "/home/shared"]
        );
    }

    #[test]
    fn extending_matches_a_full_mask() {
        let index = sample();
        let bob = Viewer { uid: 1001, groups: vec![1001, 50] };
        let scope = Scope::new(["/"], ["*.txt"]);
        let full = mask(&index, Some(&bob), Some(&scope));
        let mut partial = vec![0u64; 1];
        extend(&index, Some(&bob), Some(&scope), &mut partial, 0);
        assert_eq!(partial, full);
        for viewer in [None, Some(&bob)] {
            let mut ext = Vec::new();
            extend(&index, viewer, None, &mut ext, 0);
            assert_eq!(ext, mask(&index, viewer, None));
        }
    }

    #[test]
    fn scope_filters_too() {
        let index = sample();
        let alice = Viewer { uid: 1000, groups: vec![1000] };
        let only_pub = Scope::new(["/home/pub"], std::iter::empty::<&str>());
        assert_eq!(visible(&index, Some(&alice), Some(&only_pub)), ["/home/pub", "/home/pub/hello.txt"]);
        let no_alice = Scope::new(["/"], ["/home/alice", "*.txt"]);
        assert_eq!(
            visible(&index, Some(&alice), Some(&no_alice)),
            ["/home", "/home/acl", "/home/pub", "/home/shared"]
        );
    }
}
