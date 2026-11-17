//! The Files panel's collections: folders of remembered documents that the
//! user makes, names and nests. Purely a list — nothing on disk moves — and
//! a document in a folder no longer counts toward the recent history's cap,
//! so a folder keeps it for good.
//!
//! GTK-free so the tree operations can be unit-tested. Nodes carry a
//! session-only `id` (never persisted, assigned on load and on creation),
//! which is what the UI and drag-and-drop refer to: an index path would go
//! stale the moment a move shifts its siblings.
//!
//! Invariants: the root holds folders only (loose files *are* the recent
//! list) and a path appears at most once in the whole tree.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::settings::path_json;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Node {
    Folder(Folder),
    File(FileEntry),
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Folder {
    #[serde(skip)]
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub expanded: bool,
    #[serde(default)]
    pub children: Vec<Node>,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FileEntry {
    #[serde(skip)]
    pub id: u64,
    #[serde(with = "path_json")]
    pub path: PathBuf,
    /// Leads its folder, in pin order (the order among pinned siblings).
    #[serde(default)]
    pub pinned: bool,
}

impl Node {
    pub fn id(&self) -> u64 {
        match self {
            Node::Folder(f) => f.id,
            Node::File(f) => f.id,
        }
    }
}

/// Where a dragged item lands, relative to a target node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Place {
    Into(u64),
    Before(u64),
    After(u64),
}

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Tree {
    #[serde(default)]
    pub folders: Vec<Node>,
    #[serde(skip)]
    next_id: u64,
}

pub fn path() -> PathBuf {
    gtk4::glib::user_config_dir().join("bindle").join("collections.json")
}

impl Tree {
    /// Read `collections.json`; a missing file is an empty tree. One that
    /// does not parse is set aside as `.bad` rather than overwritten blank.
    pub fn load() -> Tree {
        let path = path();
        match fs::read(&path) {
            Ok(bytes) => Tree::from_json(&bytes).unwrap_or_else(|| {
                let _ = fs::copy(&path, path.with_extension("json.bad"));
                Tree::default()
            }),
            Err(_) => Tree::default(),
        }
    }

    pub fn from_json(bytes: &[u8]) -> Option<Tree> {
        let mut tree: Tree = serde_json::from_slice(bytes).ok()?;
        tree.sanitize();
        Some(tree)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    pub fn save(&self) {
        let path = path();
        if let Some(dir) = path.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let _ = crate::doc::write_atomic(&path, self.to_json().as_bytes());
    }

    /// Enforce the invariants on a hand-edited or old file, and number it.
    fn sanitize(&mut self) {
        self.folders.retain(|n| matches!(n, Node::Folder(_)));
        let mut seen = HashSet::new();
        dedup(&mut self.folders, &mut seen);
        let mut next = 0;
        number(&mut self.folders, &mut next);
        self.next_id = next;
    }

    fn fresh_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    pub fn is_empty(&self) -> bool {
        self.folders.is_empty()
    }

    pub fn node(&self, id: u64) -> Option<&Node> {
        find(&self.folders, id)
    }

    fn node_mut(&mut self, id: u64) -> Option<&mut Node> {
        find_mut(&mut self.folders, id)
    }

    pub fn folder(&self, id: u64) -> Option<&Folder> {
        match self.node(id) {
            Some(Node::Folder(f)) => Some(f),
            _ => None,
        }
    }

    pub fn file(&self, id: u64) -> Option<&FileEntry> {
        match self.node(id) {
            Some(Node::File(f)) => Some(f),
            _ => None,
        }
    }

    /// The entry for `path`, wherever it is.
    pub fn file_id(&self, path: &Path) -> Option<u64> {
        let mut found = None;
        walk(&self.folders, &mut |n| {
            if let Node::File(f) = n {
                if f.path == path {
                    found = Some(f.id);
                }
            }
        });
        found
    }

    pub fn contains_path(&self, path: &Path) -> bool {
        self.file_id(path).is_some()
    }

    /// Every document in the tree.
    pub fn paths(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        walk(&self.folders, &mut |n| {
            if let Node::File(f) = n {
                out.push(f.path.clone());
            }
        });
        out
    }

    /// The folders enclosing `id`, outermost first (empty for a root folder
    /// or an unknown id).
    pub fn ancestors(&self, id: u64) -> Vec<u64> {
        fn go(nodes: &[Node], id: u64, chain: &mut Vec<u64>) -> bool {
            for n in nodes {
                if n.id() == id {
                    return true;
                }
                if let Node::Folder(f) = n {
                    chain.push(f.id);
                    if go(&f.children, id, chain) {
                        return true;
                    }
                    chain.pop();
                }
            }
            false
        }
        let mut chain = Vec::new();
        if go(&self.folders, id, &mut chain) { chain } else { Vec::new() }
    }

    /// The folder directly holding `id` (None at the root).
    pub fn parent(&self, id: u64) -> Option<u64> {
        self.ancestors(id).last().copied()
    }

    /// Documents in a folder, subfolders included.
    pub fn count_files(&self, id: u64) -> usize {
        let mut n = 0;
        if let Some(f) = self.folder(id) {
            walk(&f.children, &mut |c| n += matches!(c, Node::File(_)) as usize);
        }
        n
    }

    /// Every folder, depth first in display order, with its path label
    /// ("Work / Drafts") — the "Move to" menu.
    pub fn folder_list(&self) -> Vec<(u64, String)> {
        fn go(nodes: &[Node], prefix: &str, out: &mut Vec<(u64, String)>) {
            for n in display(nodes) {
                if let Node::Folder(f) = n {
                    let label = if prefix.is_empty() {
                        f.name.clone()
                    } else {
                        format!("{prefix} / {}", f.name)
                    };
                    out.push((f.id, label.clone()));
                    go(&f.children, &label, out);
                }
            }
        }
        let mut out = Vec::new();
        go(&self.folders, "", &mut out);
        out
    }

    /// New folder at the end of `parent` (or of the root). None if the
    /// parent is not a folder.
    pub fn add_folder(&mut self, parent: Option<u64>, name: &str) -> Option<u64> {
        let id = self.fresh_id();
        let folder = Node::Folder(Folder {
            id,
            name: name.trim().to_string(),
            expanded: true,
            children: Vec::new(),
        });
        match parent {
            None => self.folders.push(folder),
            Some(p) => match self.node_mut(p) {
                Some(Node::Folder(f)) => f.children.push(folder),
                _ => return None,
            },
        }
        Some(id)
    }

    pub fn rename(&mut self, id: u64, name: &str) {
        if let Some(Node::Folder(f)) = self.node_mut(id) {
            f.name = name.trim().to_string();
        }
    }

    pub fn is_expanded(&self, id: u64) -> bool {
        self.folder(id).is_some_and(|f| f.expanded)
    }

    pub fn set_expanded(&mut self, id: u64, expanded: bool) {
        if let Some(Node::Folder(f)) = self.node_mut(id) {
            f.expanded = expanded;
        }
    }

    /// Remove a node from wherever it is.
    pub fn take(&mut self, id: u64) -> Option<Node> {
        fn go(nodes: &mut Vec<Node>, id: u64) -> Option<Node> {
            if let Some(i) = nodes.iter().position(|n| n.id() == id) {
                return Some(nodes.remove(i));
            }
            nodes.iter_mut().find_map(|n| match n {
                Node::Folder(f) => go(&mut f.children, id),
                Node::File(_) => None,
            })
        }
        go(&mut self.folders, id)
    }

    /// Delete a folder; the documents in it (and in its subfolders) are
    /// returned, so the caller can hand them back to the recent list.
    pub fn delete_folder(&mut self, id: u64) -> Vec<FileEntry> {
        let mut freed = Vec::new();
        if let Some(Node::Folder(f)) = self.node(id) {
            walk(&f.children, &mut |n| {
                if let Node::File(e) = n {
                    freed.push(e.clone());
                }
            });
        } else {
            return freed;
        }
        self.take(id);
        freed
    }

    /// Point an entry at a new path (Locate…). An entry already holding
    /// that path elsewhere gives way, so the path stays unique.
    pub fn relocate(&mut self, id: u64, path: PathBuf) {
        if let Some(other) = self.file_id(&path).filter(|&o| o != id) {
            self.take(other);
        }
        if let Some(Node::File(f)) = self.node_mut(id) {
            f.path = path;
        }
    }

    /// Pin or unpin. A new pin joins the bottom of its folder's pinned
    /// group, so the existing pins keep their order.
    pub fn set_pinned(&mut self, id: u64, pinned: bool) {
        let Some(parent) = self.parent(id) else { return };
        let Some(Node::Folder(folder)) = self.node_mut(parent) else { return };
        let kids = &mut folder.children;
        let Some(i) = kids.iter().position(|n| n.id() == id) else { return };
        let Node::File(mut entry) = kids.remove(i) else { unreachable!() };
        entry.pinned = pinned;
        let at = if pinned {
            kids.iter().rposition(is_pinned).map_or(0, |p| p + 1)
        } else {
            i
        };
        kids.insert(at.min(kids.len()), Node::File(entry));
    }

    /// Whether moving node `moving` (None: a document from the recent list)
    /// to `place` is allowed: a file never lands at the root, and a folder
    /// never inside itself.
    pub fn can_place(&self, moving: Option<u64>, place: Place) -> bool {
        let (target, into) = match place {
            Place::Into(t) => (t, true),
            Place::Before(t) | Place::After(t) => (t, false),
        };
        let Some(target_node) = self.node(target) else { return false };
        if into && !matches!(target_node, Node::Folder(_)) {
            return false;
        }
        let moving_folder = match moving {
            Some(m) => match self.node(m) {
                Some(Node::Folder(_)) => true,
                Some(Node::File(_)) => false,
                None => return false,
            },
            None => false,
        };
        if let Some(m) = moving {
            if m == target || self.ancestors(target).contains(&m) {
                return false;
            }
        }
        // Beside a root node means at the root: folders only.
        if !into && self.parent(target).is_none() && !moving_folder {
            return false;
        }
        true
    }

    /// Move an existing node. False (and nothing changed) if not allowed.
    pub fn move_node(&mut self, id: u64, place: Place) -> bool {
        if !self.can_place(Some(id), place) {
            return false;
        }
        let Some(node) = self.take(id) else { return false };
        self.insert(node, place)
    }

    /// File `path` at `place`, taking it from wherever it was in the tree.
    pub fn place_file(&mut self, path: &Path, pinned: bool, place: Place) -> bool {
        if let Some(id) = self.file_id(path) {
            return self.move_node(id, place);
        }
        if !self.can_place(None, place) {
            return false;
        }
        let id = self.fresh_id();
        let entry = Node::File(FileEntry { id, path: path.to_path_buf(), pinned });
        self.insert(entry, place)
    }

    fn insert(&mut self, node: Node, place: Place) -> bool {
        match place {
            Place::Into(t) => match self.node_mut(t) {
                Some(Node::Folder(f)) => {
                    f.children.push(node);
                    true
                }
                _ => false,
            },
            Place::Before(t) | Place::After(t) => {
                let after = matches!(place, Place::After(_));
                let siblings = match self.parent(t) {
                    None => &mut self.folders,
                    Some(p) => match self.node_mut(p) {
                        Some(Node::Folder(f)) => &mut f.children,
                        _ => return false,
                    },
                };
                let Some(i) = siblings.iter().position(|n| n.id() == t) else { return false };
                siblings.insert(i + after as usize, node);
                true
            }
        }
    }
}

/// A folder's children in display order: pinned files (pin order), then
/// subfolders, then the other files — each group in stored order.
pub fn display(children: &[Node]) -> Vec<&Node> {
    let pinned = children.iter().filter(|n| is_pinned(n));
    let folders = children.iter().filter(|n| matches!(n, Node::Folder(_)));
    let files = children.iter().filter(|n| matches!(n, Node::File(f) if !f.pinned));
    pinned.chain(folders).chain(files).collect()
}

/// Cap the recent history at `max` documents, not counting the ones `kept`
/// (filed in a folder): those stay wherever they are in the list, except
/// past the cut, where they go with the rest.
pub fn cap_recent(list: &mut Vec<PathBuf>, max: usize, kept: impl Fn(&Path) -> bool) {
    let mut counted = 0;
    let cut = list.iter().position(|p| {
        if kept(p) {
            return false;
        }
        counted += 1;
        counted > max
    });
    if let Some(cut) = cut {
        list.truncate(cut);
    }
}

fn is_pinned(n: &Node) -> bool {
    matches!(n, Node::File(f) if f.pinned)
}

fn find(nodes: &[Node], id: u64) -> Option<&Node> {
    for n in nodes {
        if n.id() == id {
            return Some(n);
        }
        if let Node::Folder(f) = n {
            if let Some(hit) = find(&f.children, id) {
                return Some(hit);
            }
        }
    }
    None
}

fn find_mut(nodes: &mut [Node], id: u64) -> Option<&mut Node> {
    for n in nodes {
        if n.id() == id {
            return Some(n);
        }
        if let Node::Folder(f) = n {
            if let Some(hit) = find_mut(&mut f.children, id) {
                return Some(hit);
            }
        }
    }
    None
}

fn walk<'a>(nodes: &'a [Node], f: &mut impl FnMut(&'a Node)) {
    for n in nodes {
        f(n);
        if let Node::Folder(folder) = n {
            walk(&folder.children, f);
        }
    }
}

fn number(nodes: &mut [Node], next: &mut u64) {
    for n in nodes {
        *next += 1;
        match n {
            Node::Folder(f) => {
                f.id = *next;
                number(&mut f.children, next);
            }
            Node::File(f) => f.id = *next,
        }
    }
}

fn dedup(nodes: &mut Vec<Node>, seen: &mut HashSet<PathBuf>) {
    nodes.retain(|n| match n {
        Node::File(f) => seen.insert(f.path.clone()),
        Node::Folder(_) => true,
    });
    for n in nodes {
        if let Node::Folder(f) = n {
            dedup(&mut f.children, seen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Work { a, Drafts { b } }, Home {}
    fn sample() -> (Tree, u64, u64, u64) {
        let mut t = Tree::default();
        let work = t.add_folder(None, "Work").unwrap();
        let drafts = t.add_folder(Some(work), "Drafts").unwrap();
        let home = t.add_folder(None, "Home").unwrap();
        assert!(t.place_file(&p("/a.md"), false, Place::Into(work)));
        assert!(t.place_file(&p("/b.md"), false, Place::Into(drafts)));
        (t, work, drafts, home)
    }

    fn names(t: &Tree, nodes: &[Node]) -> Vec<String> {
        let _ = t;
        display(nodes)
            .into_iter()
            .map(|n| match n {
                Node::Folder(f) => f.name.clone(),
                Node::File(f) => f.path.display().to_string(),
            })
            .collect()
    }

    #[test]
    fn a_path_lives_in_one_place() {
        let (mut t, work, _, home) = sample();
        assert!(t.place_file(&p("/b.md"), false, Place::Into(home)));
        assert_eq!(t.paths().iter().filter(|x| **x == p("/b.md")).count(), 1);
        assert_eq!(t.parent(t.file_id(&p("/b.md")).unwrap()), Some(home));
        assert_eq!(t.count_files(work), 1);
    }

    #[test]
    fn files_never_land_at_the_root() {
        let (mut t, work, _, home) = sample();
        assert!(!t.place_file(&p("/c.md"), false, Place::Before(work)));
        let a = t.file_id(&p("/a.md")).unwrap();
        assert!(!t.move_node(a, Place::After(home)));
        assert!(t.contains_path(&p("/a.md")));
        assert!(!t.contains_path(&p("/c.md")));
        // Beside a file inside a folder is fine.
        assert!(t.place_file(&p("/c.md"), false, Place::Before(a)));
        assert_eq!(t.parent(t.file_id(&p("/c.md")).unwrap()), Some(work));
    }

    #[test]
    fn a_folder_cannot_move_into_itself() {
        let (mut t, work, drafts, home) = sample();
        assert!(!t.move_node(work, Place::Into(work)));
        assert!(!t.move_node(work, Place::Into(drafts)));
        let b = t.file_id(&p("/b.md")).unwrap();
        assert!(!t.move_node(work, Place::Before(b)));
        assert!(t.move_node(home, Place::Into(drafts)));
        assert_eq!(t.ancestors(home), vec![work, drafts]);
        // Reorder at the root.
        assert!(t.move_node(drafts, Place::After(work)));
        let order: Vec<u64> = t.folders.iter().map(Node::id).collect();
        assert_eq!(order, vec![work, drafts]);
    }

    #[test]
    fn display_puts_pins_then_folders_then_files() {
        let (mut t, work, _, _) = sample();
        t.place_file(&p("/c.md"), false, Place::Into(work));
        t.place_file(&p("/d.md"), false, Place::Into(work));
        t.set_pinned(t.file_id(&p("/d.md")).unwrap(), true);
        t.set_pinned(t.file_id(&p("/c.md")).unwrap(), true);
        let kids = &t.folder(work).unwrap().children;
        assert_eq!(names(&t, kids), ["/d.md", "/c.md", "Drafts", "/a.md"]);
        t.set_pinned(t.file_id(&p("/d.md")).unwrap(), false);
        let kids = &t.folder(work).unwrap().children;
        assert_eq!(names(&t, kids), ["/c.md", "Drafts", "/d.md", "/a.md"]);
    }

    #[test]
    fn deleting_a_folder_frees_everything_under_it() {
        let (mut t, work, _, home) = sample();
        let freed: Vec<PathBuf> = t.delete_folder(work).into_iter().map(|e| e.path).collect();
        assert_eq!(freed, [p("/b.md"), p("/a.md")]);
        assert!(t.paths().is_empty());
        assert_eq!(t.folders.len(), 1);
        assert_eq!(t.folders[0].id(), home);
    }

    #[test]
    fn relocate_keeps_paths_unique() {
        let (mut t, _, _, _) = sample();
        let a = t.file_id(&p("/a.md")).unwrap();
        t.relocate(a, p("/b.md"));
        assert_eq!(t.paths(), [p("/b.md")]);
        assert_eq!(t.file_id(&p("/b.md")), Some(a));
    }

    #[test]
    fn json_round_trips_and_is_sanitized() {
        let (t, _, _, _) = sample();
        let back = Tree::from_json(t.to_json().as_bytes()).unwrap();
        assert_eq!(back.folders.len(), 2);
        assert_eq!(back.paths(), t.paths());
        assert_eq!(back.folder_list().iter().map(|f| f.1.clone()).collect::<Vec<_>>(),
            ["Work", "Work / Drafts", "Home"]);
        // A loose file at the root and a duplicate are dropped.
        let raw = r#"{"folders":[{"file":{"path":"/x.md"}},
            {"folder":{"name":"A","children":[{"file":{"path":"/y.md"}},{"file":{"path":"/y.md","pinned":true}}]}}]}"#;
        let t = Tree::from_json(raw.as_bytes()).unwrap();
        assert_eq!(t.paths(), [p("/y.md")]);
        // Fresh ids don't collide with loaded ones.
        let mut t = t;
        let new = t.add_folder(None, "B").unwrap();
        let mut ids = Vec::new();
        walk(&t.folders, &mut |n| ids.push(n.id()));
        assert!(ids.iter().filter(|&&i| i == new).count() == 1);
    }

    #[test]
    fn filed_documents_do_not_count_toward_the_cap() {
        let mut list: Vec<PathBuf> = (0..6).map(|i| p(&format!("/{i}"))).collect();
        let kept = |x: &Path| x == Path::new("/1") || x == Path::new("/2");
        cap_recent(&mut list, 3, kept);
        assert_eq!(list, [p("/0"), p("/1"), p("/2"), p("/3"), p("/4")]);
        let mut list: Vec<PathBuf> = (0..3).map(|i| p(&format!("/{i}"))).collect();
        cap_recent(&mut list, 3, |_| false);
        assert_eq!(list.len(), 3);
    }
}
