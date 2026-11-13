//! Independent expected-state model: worktree files, index entries, refs and
//! commits, with object IDs computed from Git's object encoding by
//! `lifecycle_support::sha`. Nothing here asks rgit or C Git for an answer.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::proc::{EMAIL, NAME, Result};
use super::sha;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hash {
    Sha1,
    Sha256,
}

impl Hash {
    pub const ALL: [Hash; 2] = [Hash::Sha1, Hash::Sha256];

    pub fn name(self) -> &'static str {
        match self {
            Hash::Sha1 => "sha1",
            Hash::Sha256 => "sha256",
        }
    }

    pub fn digest(self, data: &[u8]) -> Vec<u8> {
        match self {
            Hash::Sha1 => sha::sha1(data).to_vec(),
            Hash::Sha256 => sha::sha256(data).to_vec(),
        }
    }

    /// Raw object ID of `<kind> <len>\0<body>`.
    pub fn object(self, kind: &str, body: &[u8]) -> Vec<u8> {
        let mut data = format!("{kind} {}\0", body.len()).into_bytes();
        data.extend_from_slice(body);
        self.digest(&data)
    }

    pub fn object_hex(self, kind: &str, body: &[u8]) -> String {
        sha::hex(&self.object(kind, body))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Node {
    File { bytes: Vec<u8>, exec: bool },
    Symlink(Vec<u8>),
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entry {
    pub mode: u32,
    pub oid: String,
}

pub type Index = BTreeMap<Vec<u8>, Entry>;

impl Node {
    pub fn entry(&self, hash: Hash) -> Entry {
        match self {
            Node::File { bytes, exec } => Entry {
                mode: if *exec { 0o100755 } else { 0o100644 },
                oid: hash.object_hex("blob", bytes),
            },
            Node::Symlink(target) => Entry {
                mode: 0o120000,
                oid: hash.object_hex("blob", target),
            },
        }
    }
}

/// Serialize and hash every tree for `index`; returns the root tree ID.
pub fn tree_id(hash: Hash, index: &Index) -> String {
    /// (sort key, octal mode, name, raw object ID)
    type Item = (Vec<u8>, String, Vec<u8>, Vec<u8>);
    fn build(hash: Hash, entries: &[(&[u8], &Entry)]) -> Vec<u8> {
        // Git sorts directories as "name/".
        let mut items: Vec<Item> = Vec::new();
        let mut i = 0;
        while i < entries.len() {
            let (path, entry) = entries[i];
            if let Some(slash) = path.iter().position(|b| *b == b'/') {
                let name = &path[..slash];
                let mut children = Vec::new();
                while i < entries.len()
                    && entries[i].0.len() > slash
                    && &entries[i].0[..slash] == name
                    && entries[i].0[slash] == b'/'
                {
                    children.push((&entries[i].0[slash + 1..], entries[i].1));
                    i += 1;
                }
                let mut key = name.to_vec();
                key.push(b'/');
                items.push((key, "40000".into(), name.to_vec(), build(hash, &children)));
            } else {
                items.push((
                    path.to_vec(),
                    format!("{:o}", entry.mode),
                    path.to_vec(),
                    unhex(&entry.oid),
                ));
                i += 1;
            }
        }
        items.sort_by(|a, b| a.0.cmp(&b.0));
        let mut body = Vec::new();
        for (_, mode, name, id) in items {
            body.extend_from_slice(mode.as_bytes());
            body.push(b' ');
            body.extend_from_slice(&name);
            body.push(0);
            body.extend_from_slice(&id);
        }
        hash.object("tree", &body)
    }
    // Index order is byte order of full paths, which groups each directory's
    // descendants contiguously (all share the "dir/" prefix).
    let entries: Vec<(&[u8], &Entry)> = index.iter().map(|(p, e)| (p.as_slice(), e)).collect();
    sha::hex(&build(hash, &entries))
}

pub fn unhex(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap_or(0))
        .collect()
}

/// `-m a -m b` => "a\n\nb\n" (ADR 0003 CLI contract and C Git).
pub fn message(parts: &[&str]) -> String {
    let mut text = parts.join("\n\n");
    text.push('\n');
    text
}

pub fn commit_body(tree: &str, parents: &[String], when: i64, message: &str) -> Vec<u8> {
    let mut body = format!("tree {tree}\n");
    for parent in parents {
        body.push_str(&format!("parent {parent}\n"));
    }
    body.push_str(&format!("author {NAME} <{EMAIL}> {when} +0000\n"));
    body.push_str(&format!("committer {NAME} <{EMAIL}> {when} +0000\n\n"));
    body.push_str(message);
    body.into_bytes()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Head {
    Branch(String),
    Detached(String),
}

#[derive(Clone, Debug)]
pub struct CommitInfo {
    pub body: Vec<u8>,
    pub tree: Index,
}

#[derive(Clone, Debug)]
pub struct Checkout {
    pub root: PathBuf,
    pub worktree: BTreeMap<Vec<u8>, Node>,
    pub index: Index,
    pub head: Head,
}

/// One repository: shared refs/objects, one or more checkouts (the main
/// worktree first, then linked worktrees).
#[derive(Clone, Debug)]
pub struct Model {
    pub hash: Hash,
    pub refs: BTreeMap<String, String>,
    pub commits: BTreeMap<String, CommitInfo>,
    pub checkouts: Vec<Checkout>,
    pub clock: i64,
}

#[cfg(unix)]
pub fn os_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
}

#[cfg(not(unix))]
pub fn os_path(bytes: &[u8]) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

impl Model {
    pub fn new(hash: Hash, root: &Path, branch: &str) -> Self {
        Self {
            hash,
            refs: BTreeMap::new(),
            commits: BTreeMap::new(),
            checkouts: vec![Checkout {
                root: root.to_path_buf(),
                worktree: BTreeMap::new(),
                index: Index::new(),
                head: Head::Branch(branch.into()),
            }],
            clock: 1_767_225_600,
        }
    }

    pub fn root(&self, c: usize) -> &Path {
        &self.checkouts[c].root
    }

    pub fn tick(&mut self) -> i64 {
        self.clock += 60;
        self.clock
    }

    /// Write a regular file through the model, mirroring it to disk.
    pub fn write(&mut self, c: usize, path: &[u8], bytes: &[u8]) -> Result {
        let exec = match self.checkouts[c].worktree.get(path) {
            Some(Node::File { exec, .. }) => *exec,
            _ => false,
        };
        let full = self.checkouts[c].root.join(os_path(path));
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::symlink_metadata(&full).is_ok_and(|m| m.file_type().is_symlink()) {
            fs::remove_file(&full)?;
        }
        fs::write(&full, bytes)?;
        self.checkouts[c].worktree.insert(
            path.to_vec(),
            Node::File {
                bytes: bytes.to_vec(),
                exec,
            },
        );
        if exec {
            self.chmod(c, path, true)?;
        }
        Ok(())
    }

    /// Record bytes that reached the file by other means (e.g. a mapping).
    pub fn observe_bytes(&mut self, c: usize, path: &[u8], bytes: &[u8]) -> Result {
        match self.checkouts[c].worktree.get_mut(path) {
            Some(Node::File { bytes: current, .. }) => {
                *current = bytes.to_vec();
                Ok(())
            }
            _ => Err("observe_bytes on a non-file".into()),
        }
    }

    #[cfg(unix)]
    pub fn chmod(&mut self, c: usize, path: &[u8], exec: bool) -> Result {
        use std::os::unix::fs::PermissionsExt;
        let full = self.checkouts[c].root.join(os_path(path));
        fs::set_permissions(
            &full,
            fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }),
        )?;
        match self.checkouts[c].worktree.get_mut(path) {
            Some(Node::File { exec: flag, .. }) => {
                *flag = exec;
                Ok(())
            }
            _ => Err("chmod on a non-file".into()),
        }
    }

    #[cfg(not(unix))]
    pub fn chmod(&mut self, _c: usize, _path: &[u8], _exec: bool) -> Result {
        Err("exec bit requires Unix".into())
    }

    #[cfg(unix)]
    pub fn symlink(&mut self, c: usize, path: &[u8], target: &[u8]) -> Result {
        let full = self.checkouts[c].root.join(os_path(path));
        if let Some(parent) = full.parent() {
            fs::create_dir_all(parent)?;
        }
        if fs::symlink_metadata(&full).is_ok() {
            fs::remove_file(&full)?;
        }
        std::os::unix::fs::symlink(os_path(target), &full)?;
        self.checkouts[c]
            .worktree
            .insert(path.to_vec(), Node::Symlink(target.to_vec()));
        Ok(())
    }

    pub fn remove(&mut self, c: usize, path: &[u8]) -> Result {
        fs::remove_file(self.checkouts[c].root.join(os_path(path)))?;
        self.checkouts[c].worktree.remove(path);
        Ok(())
    }

    /// C Git `add` semantics for literal pathspecs: a file stages that file; a
    /// directory (or ".") stages every file beneath it and removals of
    /// tracked paths beneath it; a missing tracked path stages its removal.
    pub fn stage(&mut self, c: usize, pathspecs: &[&[u8]]) -> Result {
        let hash = self.hash;
        let checkout = &mut self.checkouts[c];
        for spec in pathspecs {
            let under = |path: &[u8]| {
                *spec == b"."
                    || path == *spec
                    || (path.len() > spec.len()
                        && path.starts_with(spec)
                        && path[spec.len()] == b'/')
            };
            let present: Vec<_> = checkout
                .worktree
                .iter()
                .filter(|(p, _)| under(p))
                .map(|(p, n)| (p.clone(), n.entry(hash)))
                .collect();
            let removed: Vec<_> = checkout
                .index
                .keys()
                .filter(|p| under(p) && !checkout.worktree.contains_key(*p))
                .cloned()
                .collect();
            if present.is_empty() && removed.is_empty() {
                return Err(format!("model: pathspec {:?} matches nothing", spec).into());
            }
            for path in removed {
                checkout.index.remove(&path);
            }
            for (path, entry) in present {
                // A file replaces any index entries beneath a same-named dir
                // and vice versa (D/F conflicts are not modelled further).
                checkout.index.insert(path, entry);
            }
        }
        Ok(())
    }

    pub fn head_commit(&self, c: usize) -> Option<String> {
        match &self.checkouts[c].head {
            Head::Branch(name) => self.refs.get(name).cloned(),
            Head::Detached(oid) => Some(oid.clone()),
        }
    }

    /// Predict the commit `commit -m <parts>` creates at time `when` and
    /// advance HEAD (branch or detached) to it.
    pub fn commit(&mut self, c: usize, parts: &[&str], when: i64) -> String {
        let (oid, info) = self.predict_commit(c, parts, when);
        self.commits.insert(oid.clone(), info);
        self.advance(c, &oid);
        oid
    }

    pub fn predict_commit(&self, c: usize, parts: &[&str], when: i64) -> (String, CommitInfo) {
        let index = self.checkouts[c].index.clone();
        let tree = tree_id(self.hash, &index);
        let parents: Vec<String> = self.head_commit(c).into_iter().collect();
        let body = commit_body(&tree, &parents, when, &message(parts));
        let oid = self.hash.object_hex("commit", &body);
        (oid, CommitInfo { body, tree: index })
    }

    pub fn record_commit(&mut self, oid: &str, info: CommitInfo) {
        self.commits.insert(oid.to_owned(), info);
    }

    pub fn advance(&mut self, c: usize, oid: &str) {
        match &mut self.checkouts[c].head {
            Head::Branch(name) => {
                let name = name.clone();
                self.refs.insert(name, oid.to_owned());
            }
            Head::Detached(current) => *current = oid.to_owned(),
        }
    }

    /// Mirror `git checkout --detach` (fixture operation).
    pub fn detach(&mut self, c: usize) -> Result {
        let oid = self.head_commit(c).ok_or("cannot detach an unborn HEAD")?;
        self.checkouts[c].head = Head::Detached(oid);
        Ok(())
    }

    /// Mirror `git checkout --orphan <branch>`: index and worktree kept.
    pub fn orphan(&mut self, c: usize, branch: &str) {
        self.checkouts[c].head = Head::Branch(branch.into());
    }

    /// Mirror `git worktree add -b <branch> <root> <c's HEAD>`.
    pub fn add_linked(&mut self, c: usize, root: &Path, branch: &str) -> Result<usize> {
        let oid = self.head_commit(c).ok_or("unborn")?;
        let tree = self.commits.get(&oid).ok_or("unknown commit")?.tree.clone();
        let worktree = self.checkouts[c]
            .worktree
            .iter()
            .filter(|(p, _)| tree.contains_key(*p))
            .map(|(p, n)| (p.clone(), n.clone()))
            .collect::<BTreeMap<_, _>>();
        // Linked worktrees check out the committed tree, which must equal the
        // committed worktree content in these scenarios.
        for (path, node) in &worktree {
            if node.entry(self.hash) != tree[path] {
                return Err("linked checkout model requires a clean tree".into());
            }
        }
        self.refs.insert(branch.into(), oid);
        self.checkouts.push(Checkout {
            root: root.to_path_buf(),
            worktree,
            index: tree,
            head: Head::Branch(branch.into()),
        });
        Ok(self.checkouts.len() - 1)
    }

    /// Expected `status --porcelain=v1 -z --untracked-files=all` bytes.
    pub fn status(&self, c: usize) -> Vec<u8> {
        let checkout = &self.checkouts[c];
        let empty = Index::new();
        let head = self
            .head_commit(c)
            .and_then(|oid| self.commits.get(&oid))
            .map_or(&empty, |info| &info.tree);
        let hash = self.hash;
        let kind = |mode: u32| mode & 0o170000;
        let mut paths: Vec<&Vec<u8>> = head.keys().chain(checkout.index.keys()).collect();
        paths.sort();
        paths.dedup();
        let mut out = Vec::new();
        for path in paths {
            let x = match (head.get(path), checkout.index.get(path)) {
                (None, Some(_)) => b'A',
                (Some(_), None) => b'D',
                (Some(a), Some(b)) if a == b => b' ',
                (Some(a), Some(b)) if kind(a.mode) != kind(b.mode) => b'T',
                (Some(_), Some(_)) => b'M',
                (None, None) => continue,
            };
            let y = match checkout.index.get(path) {
                None => b' ',
                Some(staged) => match checkout.worktree.get(path).map(|n| n.entry(hash)) {
                    None => b'D',
                    Some(actual) if actual == *staged => b' ',
                    Some(actual) if kind(actual.mode) != kind(staged.mode) => b'T',
                    Some(_) => b'M',
                },
            };
            if (x, y) != (b' ', b' ') {
                out.extend_from_slice(&[x, y, b' ']);
                out.extend_from_slice(path);
                out.push(0);
            }
        }
        for path in checkout.worktree.keys() {
            if !checkout.index.contains_key(path) {
                out.extend_from_slice(b"?? ");
                out.extend_from_slice(path);
                out.push(0);
            }
        }
        out
    }
}
