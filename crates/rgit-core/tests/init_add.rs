//! init, write_blob and add, with C Git as the independent oracle: C Git
//! validates every repository rgit creates, and `add` results are compared
//! with `git add` on an identical twin worktree.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use common::{Result, fsck, git, git_command};
use rgit_core::write::{InitOptions, ObjectFormat, init};
use rgit_core::{BStr, Error, Repository};
use tempfile::{TempDir, tempdir};

const FORMATS: [ObjectFormat; 2] = [ObjectFormat::Sha1, ObjectFormat::Sha256];

fn format_name(format: ObjectFormat) -> &'static str {
    match format {
        ObjectFormat::Sha1 => "sha1",
        ObjectFormat::Sha256 => "sha256",
    }
}

fn options(format: ObjectFormat, bare: bool) -> InitOptions {
    InitOptions {
        object_format: format,
        bare,
        ..InitOptions::default()
    }
}

fn stdout(root: &Path, args: &[&str]) -> Result<Vec<u8>> {
    Ok(git(root, args)?.stdout)
}

fn text(root: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(stdout(root, args)?)?
        .trim_end()
        .to_owned())
}

/// Every file below `dir` with its bytes; detects any object/index change.
fn snapshot(dir: &Path) -> Result<BTreeMap<PathBuf, Vec<u8>>> {
    let mut result = BTreeMap::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(directory) = pending.pop() {
        for child in fs::read_dir(&directory)? {
            let child = child?;
            let kind = child.file_type()?;
            if kind.is_dir() {
                pending.push(child.path());
            } else if kind.is_file() {
                result.insert(child.path(), fs::read(child.path())?);
            } else {
                result.insert(child.path(), Vec::new());
            }
        }
    }
    Ok(result)
}

/// The refusal message carried by `Error::Other` (the `AddRefusal` type is
/// not re-exported from `rgit_core::write` yet).
fn refusal(error: &Error) -> Option<String> {
    match error {
        Error::Other(source) => Some(source.to_string()),
        _ => None,
    }
}

fn add(
    repo: &Repository,
    paths: &[&[u8]],
) -> std::result::Result<rgit_core::write::AddOutcome, Error> {
    let paths: Vec<&BStr> = paths.iter().map(|path| BStr::new(*path)).collect();
    repo.add(&paths)
}

// ---------------------------------------------------------------- init ---

#[test]
fn init_worktree_is_valid_for_c_git_in_both_hashes() -> Result {
    for format in FORMATS {
        let temp = tempdir()?;
        let root = temp.path().join("new/repo");
        let repo = init(&root, &options(format, false))?;
        assert!(!repo.is_bare());
        assert_eq!(
            fs::canonicalize(repo.git_dir())?,
            fs::canonicalize(root.join(".git"))?
        );

        assert_eq!(
            text(&root, &["rev-parse", "--show-object-format"])?,
            format_name(format)
        );
        assert_eq!(
            text(&root, &["rev-parse", "--is-bare-repository"])?,
            "false"
        );
        assert_eq!(text(&root, &["symbolic-ref", "HEAD"])?, "refs/heads/master");
        assert_eq!(text(&root, &["status", "--porcelain"])?, "");
        fsck(&root)?;
        assert!(!root.join(".git/hooks").exists(), "no hooks are written");
        assert_no_init_temporaries(&root)?;
        assert_eq!(fs::read_dir(&root)?.count(), 1, "only .git is created");

        // Same effective configuration as C Git's own init on this machine.
        let twin = temp.path().join("twin");
        git(
            temp.path(),
            &["init", "-q", "--object-format", format_name(format), "twin"],
        )?;
        assert_eq!(
            text(&root, &["config", "--local", "--list"])?,
            text(&twin, &["config", "--local", "--list"])?
        );

        fs::write(root.join("file"), b"content\n")?;
        git(&root, &["add", "file"])?;
        git(&root, &["commit", "-q", "-m", "first"])?;
        fsck(&root)?;
        assert_eq!(text(&root, &["rev-list", "--count", "HEAD"])?, "1");
    }
    Ok(())
}

#[test]
fn init_bare_is_valid_for_c_git_in_both_hashes() -> Result {
    for format in FORMATS {
        let temp = tempdir()?;
        // Absent target, and an existing empty directory.
        let absent = temp.path().join("absent.git");
        let empty = temp.path().join("empty.git");
        fs::create_dir(&empty)?;
        for root in [&absent, &empty] {
            let repo = init(root, &options(format, true))?;
            assert!(repo.is_bare());
            assert_eq!(text(root, &["rev-parse", "--is-bare-repository"])?, "true");
            assert_eq!(
                text(root, &["rev-parse", "--show-object-format"])?,
                format_name(format)
            );
            fsck(root)?;
            assert!(!root.join("hooks").exists());
            let twin = temp
                .path()
                .join(format!("twin-{}", root.display().to_string().len()));
            git(
                temp.path(),
                &[
                    "init",
                    "-q",
                    "--bare",
                    "--object-format",
                    format_name(format),
                    &twin.to_string_lossy(),
                ],
            )?;
            assert_eq!(
                text(root, &["config", "--local", "--list"])?,
                text(&twin, &["config", "--local", "--list"])?
            );
        }
        assert_no_init_temporaries(temp.path())?;
        // A bare repository accepts a push-equivalent: commit elsewhere and fetch.
        let work = temp.path().join("work");
        git(
            temp.path(),
            &["init", "-q", "--object-format", format_name(format), "work"],
        )?;
        fs::write(work.join("f"), b"x")?;
        git(&work, &["add", "f"])?;
        git(&work, &["commit", "-q", "-m", "c"])?;
        git(
            &work,
            &[
                "push",
                "-q",
                &absent.to_string_lossy(),
                "HEAD:refs/heads/master",
            ],
        )?;
        fsck(&absent)?;
    }
    Ok(())
}

fn assert_no_init_temporaries(dir: &Path) -> Result {
    for child in fs::read_dir(dir)? {
        let name = child?.file_name();
        assert!(
            !name.to_string_lossy().contains("rgit-init"),
            "temporary left behind: {name:?}"
        );
    }
    Ok(())
}

#[test]
fn init_refuses_existing_repositories_without_change() -> Result {
    let temp = tempdir()?;
    let root = temp.path().join("repo");
    init(&root, &InitOptions::default())?;
    let before = snapshot(&root)?;
    assert!(matches!(
        init(&root, &InitOptions::default()),
        Err(Error::AlreadyExists(_))
    ));
    // A C Git repository, a gitfile and a bare repository are all refused.
    let c_git = temp.path().join("c");
    git(temp.path(), &["init", "-q", "c"])?;
    assert!(matches!(
        init(&c_git, &InitOptions::default()),
        Err(Error::AlreadyExists(_))
    ));
    let gitfile = temp.path().join("linked");
    fs::create_dir(&gitfile)?;
    fs::write(gitfile.join(".git"), b"gitdir: /nonexistent\n")?;
    assert!(matches!(
        init(&gitfile, &InitOptions::default()),
        Err(Error::AlreadyExists(_))
    ));
    let bare = temp.path().join("bare.git");
    init(&bare, &options(ObjectFormat::Sha1, true))?;
    let bare_before = snapshot(&bare)?;
    assert!(matches!(
        init(&bare, &options(ObjectFormat::Sha1, true)),
        Err(Error::AlreadyExists(_))
    ));
    let occupied = temp.path().join("occupied");
    fs::create_dir(&occupied)?;
    fs::write(occupied.join("x"), b"x")?;
    assert!(matches!(
        init(&occupied, &options(ObjectFormat::Sha1, true)),
        Err(Error::AlreadyExists(_))
    ));
    assert_eq!(snapshot(&root)?, before);
    assert_eq!(snapshot(&bare)?, bare_before);
    assert_eq!(fs::read_dir(&occupied)?.count(), 1);
    assert_no_init_temporaries(temp.path())?;
    Ok(())
}

#[test]
fn init_branch_names_follow_c_git() -> Result {
    let temp = tempdir()?;
    for (index, name) in [
        "main", "a/b", "HEAD", "@", "-x", "a..b", "a.lock", "x/", "a//b", ".x", "a b", "",
    ]
    .iter()
    .enumerate()
    {
        let ours = temp.path().join(format!("ours{index}"));
        let theirs = temp.path().join(format!("theirs{index}"));
        let c_git = git_command(temp.path())
            .args(["init", "-q", "-b", name])
            .arg(&theirs)
            .output()?;
        let result = init(
            &ours,
            &InitOptions {
                initial_branch: name.as_bytes().to_vec(),
                ..InitOptions::default()
            },
        );
        match result {
            Ok(_) => {
                assert!(c_git.status.success(), "{name:?} accepted only by rgit");
                assert_eq!(
                    fs::read(ours.join(".git/HEAD"))?,
                    fs::read(theirs.join(".git/HEAD"))?
                );
            }
            Err(Error::InvalidRefName(_)) => {
                assert!(!c_git.status.success(), "{name:?} refused only by rgit");
                assert!(!ours.exists(), "nothing is created before validation");
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

// ---------------------------------------------------------- write_blob ---

#[test]
fn write_blob_matches_git_hash_object_in_both_hashes() -> Result {
    let mut large = Vec::with_capacity(1_500_000);
    let mut state = 0x2545_f491_4f6c_dd1du64;
    while large.len() < 1_500_000 {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        large.extend_from_slice(&state.to_le_bytes());
    }
    let binary: Vec<u8> = (0..=255u8).cycle().take(4096).collect();
    let samples: [&[u8]; 4] = [b"", b"hello\n", &binary, &large];
    for format in FORMATS {
        let temp = tempdir()?;
        let root = temp.path().join("repo");
        let repo = init(&root, &options(format, false))?;
        for data in samples {
            let id = repo.write_blob(data)?;
            let expected = git_command(&root)
                .args(["hash-object", "--stdin"])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.take().map(|mut stdin| stdin.write_all(data));
                    child.wait_with_output()
                })?;
            assert_eq!(id.to_string(), String::from_utf8(expected.stdout)?.trim());
            assert_eq!(stdout(&root, &["cat-file", "blob", &id.to_string()])?, data);
            // Idempotent: an existing object is not rewritten.
            let hex = id.to_string();
            let path = root.join(".git/objects").join(&hex[..2]).join(&hex[2..]);
            let bytes = fs::read(&path)?;
            assert_eq!(repo.write_blob(data)?, id);
            assert_eq!(fs::read(&path)?, bytes);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o444);
            }
            for child in fs::read_dir(path.parent().ok_or("parent")?)? {
                let name = child?.file_name();
                assert!(!name.to_string_lossy().starts_with("tmp_obj_"));
            }
        }
        git(&root, &["fsck", "--strict", "--no-progress"])?;
    }
    Ok(())
}

// ----------------------------------------------------------------- add ---

/// Two repositories with identical worktrees: `ours` is staged by rgit and
/// `theirs` by C Git.
struct Twin {
    _temp: TempDir,
    ours: PathBuf,
    theirs: PathBuf,
}

impl Twin {
    fn new(format: ObjectFormat) -> Result<Self> {
        let temp = tempdir()?;
        let ours = temp.path().join("ours");
        let theirs = temp.path().join("theirs");
        init(&ours, &options(format, false))?;
        git(
            temp.path(),
            &[
                "init",
                "-q",
                "--object-format",
                format_name(format),
                "theirs",
            ],
        )?;
        quiet_maintenance(&ours)?;
        quiet_maintenance(&theirs)?;
        Ok(Self {
            _temp: temp,
            ours,
            theirs,
        })
    }

    fn each(&self, mut action: impl FnMut(&Path) -> Result) -> Result {
        action(&self.ours)?;
        action(&self.theirs)
    }

    fn write(&self, path: &str, data: &[u8]) -> Result {
        self.each(|root| {
            let path = root.join(path);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(path, data)?;
            Ok(())
        })
    }

    fn git(&self, args: &[&str]) -> Result {
        self.each(|root| git(root, args).map(drop))
    }

    fn repo(&self) -> Result<Repository> {
        Ok(Repository::discover(&self.ours)?)
    }

    /// rgit add in ours, git add in theirs, then compare staged state.
    fn add(&self, paths: &[&str]) -> Result<rgit_core::write::AddOutcome> {
        let outcome = add(
            &self.repo()?,
            &paths.iter().map(|p| p.as_bytes()).collect::<Vec<_>>(),
        )?;
        let mut args = vec!["add", "--"];
        args.extend_from_slice(paths);
        git(&self.theirs, &args)?;
        self.assert_same()?;
        Ok(outcome)
    }

    fn assert_same(&self) -> Result {
        assert_eq!(
            String::from_utf8_lossy(&stdout(&self.ours, &["ls-files", "-s"])?),
            String::from_utf8_lossy(&stdout(&self.theirs, &["ls-files", "-s"])?),
        );
        assert_eq!(
            text(&self.ours, &["write-tree"])?,
            text(&self.theirs, &["write-tree"])?
        );
        assert_eq!(
            text(&self.ours, &["status", "--porcelain", "-uall"])?,
            text(&self.theirs, &["status", "--porcelain", "-uall"])?
        );
        fsck(&self.ours)?;
        Ok(())
    }
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

#[test]
fn add_matches_c_git_for_files_dirs_modes_and_symlinks() -> Result {
    for format in FORMATS {
        let twin = Twin::new(format)?;
        twin.write("top.txt", b"top\n")?;
        twin.write("dir/nested/deep.bin", &[0, 1, 2, 255, b'\r', b'\n'])?;
        twin.write("dir/other.txt", b"other\n")?;
        twin.write("dir/empty", b"")?;
        twin.write("dir/sp ace & ünïcode.txt", b"name\n")?;
        twin.write("big.bin", &vec![7u8; 2 * 1024 * 1024])?;
        #[cfg(unix)]
        twin.each(|root| {
            set_executable(&root.join("dir/nested/deep.bin"))?;
            std::os::unix::fs::symlink("top.txt", root.join("link"))?;
            std::os::unix::fs::symlink("../missing/target", root.join("dir/dangling"))?;
            fs::create_dir(root.join("emptydir"))?;
            Ok(())
        })?;
        let outcome = twin.add(&["."])?;
        assert!(outcome.removed.is_empty());
        assert!(
            outcome
                .staged
                .iter()
                .any(|path| path == b"dir/nested/deep.bin")
        );
        let mut sorted = outcome.staged.clone();
        sorted.sort();
        assert_eq!(sorted, outcome.staged, "index order");
        #[cfg(unix)]
        assert!(text(&twin.ours, &["ls-files", "-s", "link"])?.starts_with("120000 "));
        // C Git commits the rgit index; the commit tree equals the twin's.
        twin.git(&["commit", "-q", "-m", "first"])?;
        assert_eq!(
            text(&twin.ours, &["rev-parse", "HEAD^{tree}"])?,
            text(&twin.theirs, &["rev-parse", "HEAD^{tree}"])?
        );
        // Re-adding unchanged content is a no-op in content terms.
        assert_eq!(twin.add(&["."])?, Default::default());
    }
    Ok(())
}

#[test]
fn add_stages_modifications_deletions_and_type_changes_like_c_git() -> Result {
    let twin = Twin::new(ObjectFormat::Sha1)?;
    twin.write("keep.txt", b"keep\n")?;
    twin.write("gone.txt", b"gone\n")?;
    twin.write("d/a.txt", b"a\n")?;
    twin.write("d/sub/b.txt", b"b\n")?;
    twin.write("becomes_dir", b"file\n")?;
    twin.write("dir_becomes_file/x", b"x\n")?;
    twin.write("mode.sh", b"#!/bin/sh\n")?;
    twin.git(&["add", "."])?;
    twin.git(&["commit", "-q", "-m", "base"])?;

    twin.write("keep.txt", b"changed\n")?;
    twin.each(|root| {
        fs::remove_file(root.join("gone.txt"))?;
        fs::remove_file(root.join("d/sub/b.txt"))?;
        fs::remove_file(root.join("becomes_dir"))?;
        fs::create_dir(root.join("becomes_dir"))?;
        fs::write(root.join("becomes_dir/inner"), b"inner\n")?;
        fs::remove_dir_all(root.join("dir_becomes_file"))?;
        fs::write(root.join("dir_becomes_file"), b"now a file\n")?;
        #[cfg(unix)]
        set_executable(&root.join("mode.sh"))?;
        Ok(())
    })?;
    // Deletions under an explicit directory, then the whole tree.
    let outcome = twin.add(&["d"])?;
    assert_eq!(outcome.removed, vec![b"d/sub/b.txt".to_vec()]);
    let outcome = twin.add(&["."])?;
    assert!(outcome.removed.contains(&b"gone.txt".to_vec()));
    assert!(outcome.removed.contains(&b"becomes_dir".to_vec()));
    assert!(outcome.removed.contains(&b"dir_becomes_file/x".to_vec()));
    assert!(outcome.staged.contains(&b"becomes_dir/inner".to_vec()));
    #[cfg(unix)]
    assert!(outcome.staged.contains(&b"mode.sh".to_vec()));
    twin.git(&["commit", "-q", "-m", "second"])?;
    assert_eq!(
        text(&twin.ours, &["rev-parse", "HEAD^{tree}"])?,
        text(&twin.theirs, &["rev-parse", "HEAD^{tree}"])?
    );
    Ok(())
}

#[test]
fn add_explicit_paths_replace_directory_file_conflicts_like_c_git() -> Result {
    let twin = Twin::new(ObjectFormat::Sha1)?;
    twin.write("a", b"file\n")?;
    twin.write("b/c", b"c\n")?;
    twin.git(&["add", "."])?;
    twin.each(|root| {
        fs::remove_file(root.join("a"))?;
        fs::create_dir(root.join("a"))?;
        fs::write(root.join("a/child"), b"child\n")?;
        fs::remove_dir_all(root.join("b"))?;
        fs::write(root.join("b"), b"b file\n")?;
        Ok(())
    })?;
    let outcome = twin.add(&["a/child"])?;
    assert_eq!(outcome.removed, vec![b"a".to_vec()]);
    let outcome = twin.add(&["b"])?;
    assert_eq!(outcome.removed, vec![b"b/c".to_vec()]);
    Ok(())
}

#[test]
fn add_skips_ignored_untracked_files_but_refreshes_tracked_ones() -> Result {
    let twin = Twin::new(ObjectFormat::Sha1)?;
    twin.write(".gitignore", b"*.log\nbuild/\n")?;
    twin.write("src/main.rs", b"fn main() {}\n")?;
    twin.write("src/debug.log", b"log\n")?;
    twin.write("build/tracked.txt", b"v1\n")?;
    twin.git(&["add", ".gitignore", "src"])?;
    twin.git(&["add", "-f", "build/tracked.txt"])?;
    twin.git(&["commit", "-q", "-m", "base"])?;
    twin.write("build/tracked.txt", b"v2\n")?;
    twin.write("build/untracked.txt", b"new\n")?;
    twin.write("src/lib.rs", b"// lib\n")?;
    twin.write("nested/.gitignore", b"!keep.log\n")?;
    twin.write("nested/keep.log", b"keep\n")?;
    twin.write("nested/drop.log", b"drop\n")?;
    let outcome = twin.add(&["."])?;
    assert!(outcome.staged.contains(&b"build/tracked.txt".to_vec()));
    assert!(outcome.staged.contains(&b"nested/keep.log".to_vec()));
    assert!(!outcome.staged.contains(&b"build/untracked.txt".to_vec()));
    assert!(!outcome.staged.contains(&b"nested/drop.log".to_vec()));
    // A tracked file that itself matches an ignore pattern is staged (C Git
    // exits 0). Naming a path at or below an ignored directory makes C Git
    // stage tracked content yet exit 1; rgit refuses without any change.
    twin.write("tracked.log", b"t1\n")?;
    twin.git(&["add", "-f", "tracked.log"])?;
    twin.write("tracked.log", b"t2\n")?;
    twin.add(&["tracked.log"])?;
    twin.write("build/tracked.txt", b"v3\n")?;
    for spec in ["build/tracked.txt", "build"] {
        let c_git = git_command(&twin.theirs).args(["add", spec]).output()?;
        assert_eq!(c_git.status.code(), Some(1));
        assert_refused(&twin.ours, &[spec.as_bytes()], is_ignored(spec))?;
    }
    Ok(())
}

#[test]
fn add_replaces_intent_to_add_and_keeps_text_auto_identity_content() -> Result {
    let twin = Twin::new(ObjectFormat::Sha256)?;
    twin.write(".gitattributes", b"* text=auto\n*.bin binary\n")?;
    twin.write("lf.txt", b"one\ntwo\n")?;
    twin.write("crlf.bin", b"one\r\ntwo\r\n")?;
    twin.write("later.txt", b"later\n")?;
    twin.git(&["add", "-N", "later.txt"])?;
    let outcome = twin.add(&["."])?;
    assert!(outcome.staged.contains(&b"later.txt".to_vec()));
    assert!(outcome.staged.contains(&b"crlf.bin".to_vec()));
    Ok(())
}

#[test]
fn add_keeps_mode_when_filemode_is_untrusted() -> Result {
    let twin = Twin::new(ObjectFormat::Sha1)?;
    twin.write("run.sh", b"#!/bin/sh\n")?;
    twin.write("plain.txt", b"plain\n")?;
    #[cfg(unix)]
    twin.each(|root| set_executable(&root.join("run.sh")))?;
    twin.git(&["add", "."])?;
    twin.git(&["config", "core.filemode", "false"])?;
    twin.each(|root| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.join("run.sh"), fs::Permissions::from_mode(0o644))?;
            set_executable(&root.join("plain.txt"))?;
        }
        fs::write(root.join("run.sh"), b"#!/bin/sh\necho\n")?;
        fs::write(root.join("plain.txt"), b"plain 2\n")?;
        fs::write(root.join("new.sh"), b"new\n")?;
        #[cfg(unix)]
        set_executable(&root.join("new.sh"))?;
        Ok(())
    })?;
    twin.add(&["."])?;
    Ok(())
}

#[test]
fn add_invalidates_cache_tree_so_c_git_commits_the_right_tree() -> Result {
    let twin = Twin::new(ObjectFormat::Sha1)?;
    for path in ["a/x/1", "a/x/2", "a/y/3", "b/4", "c"] {
        twin.write(path, path.as_bytes())?;
    }
    twin.git(&["add", "."])?;
    twin.git(&["commit", "-q", "-m", "base"])?;
    // C Git's commit leaves a fully valid cache tree in both indexes.
    twin.write("a/x/2", b"changed")?;
    twin.write("a/y/new", b"new")?;
    twin.each(|root| Ok(fs::remove_file(root.join("b/4"))?))?;
    twin.add(&["a/x/2"])?;
    twin.add(&["a/y"])?;
    twin.add(&["b"])?;
    twin.git(&["commit", "-q", "-m", "next"])?;
    assert_eq!(
        text(&twin.ours, &["rev-parse", "HEAD^{tree}"])?,
        text(&twin.theirs, &["rev-parse", "HEAD^{tree}"])?
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn add_preserves_non_utf8_names_where_the_filesystem_allows() -> Result {
    use std::os::unix::ffi::OsStrExt;
    let twin = Twin::new(ObjectFormat::Sha1)?;
    let name = std::ffi::OsStr::from_bytes(b"latin1-\xe9.txt");
    if let Err(error) = fs::write(twin.ours.join(name), b"bytes\n") {
        // APFS and HFS+ reject names that are not valid UTF-8 (EILSEQ).
        eprintln!("skipping non-UTF-8 name test: {error}");
        return Ok(());
    }
    fs::write(twin.theirs.join(name), b"bytes\n")?;
    twin.add(&["."])?;
    let outcome = add(&twin.repo()?, &[b"latin1-\xe9.txt"])?;
    assert_eq!(outcome, Default::default());
    Ok(())
}

#[test]
fn add_protects_racily_clean_entries_it_does_not_restage() -> Result {
    let temp = tempdir()?;
    let root = temp.path().join("repo");
    init(&root, &InitOptions::default())?;
    git(&root, &["config", "core.trustctime", "false"])?;
    let racy = root.join("racy.txt");
    fs::write(&racy, b"aaaa")?;
    let past = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
    fs::File::options()
        .write(true)
        .open(&racy)?
        .set_modified(past)?;
    git(&root, &["add", "racy.txt"])?;
    // Make the entry racy: the index is no newer than the file.
    fs::File::options()
        .write(true)
        .open(root.join(".git/index"))?
        .set_modified(past)?;
    // Same size, same mtime, same inode, ctime untrusted: only content differs.
    fs::write(&racy, b"bbbb")?;
    fs::File::options()
        .write(true)
        .open(&racy)?
        .set_modified(past)?;
    fs::write(root.join("other.txt"), b"other\n")?;
    add(&Repository::discover(&root)?, &[b"other.txt"])?;
    let status = text(&root, &["status", "--porcelain"])?;
    assert!(
        status.contains("AM racy.txt") || status.contains(" M racy.txt"),
        "{status}"
    );
    Ok(())
}

// ------------------------------------------------------------ refusals ---

/// Assert `add(paths)` fails as `check` expects and changed nothing under .git.
fn assert_refused(root: &Path, paths: &[&[u8]], check: impl Fn(&Error) -> bool) -> Result {
    let before = snapshot(&root.join(".git"))?;
    let result = add(&Repository::discover(root)?, paths);
    match result {
        Err(error) => assert!(check(&error), "unexpected error for {paths:?}: {error:?}"),
        Ok(outcome) => panic!("{paths:?} was accepted: {outcome:?}"),
    }
    let after = snapshot(&root.join(".git"))?;
    let changed: Vec<_> = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert!(changed.is_empty(), "{paths:?} changed .git: {changed:?}");
    Ok(())
}

fn is(expected: &'static str) -> impl Fn(&Error) -> bool {
    move |error| refusal(error).as_deref() == Some(expected)
}

fn is_ignored(path: &str) -> impl Fn(&Error) -> bool {
    let expected = format!("path '{path}' is ignored by one of your .gitignore files");
    move |error| refusal(error).as_deref() == Some(expected.as_str())
}

fn conversion(error: &Error) -> bool {
    refusal(error).is_some_and(|message| message.contains("which rgit does not perform"))
}

/// C Git's detached auto-maintenance after `commit` writes under `.git`
/// asynchronously, which would race the unchanged-state snapshots.
fn quiet_maintenance(root: &Path) -> Result {
    git(root, &["config", "maintenance.auto", "false"])?;
    git(root, &["config", "gc.auto", "0"])?;
    Ok(())
}

fn committed_repo(temp: &TempDir) -> Result<PathBuf> {
    let root = temp.path().join("repo");
    init(&root, &InitOptions::default())?;
    quiet_maintenance(&root)?;
    fs::write(root.join("tracked.txt"), b"tracked\n")?;
    fs::create_dir(root.join("dir"))?;
    fs::write(root.join("dir/file.txt"), b"file\n")?;
    git(&root, &["add", "."])?;
    git(&root, &["commit", "-q", "-m", "base"])?;
    fs::write(root.join("new.txt"), b"new\n")?;
    Ok(root)
}

#[test]
fn add_refuses_bad_pathspecs_before_any_write() -> Result {
    let temp = tempdir()?;
    let root = committed_repo(&temp)?;
    assert_refused(
        &root,
        &[b"new.txt", b"missing"],
        is("pathspec 'missing' did not match any files"),
    )?;
    assert_refused(
        &root,
        &[b"*.txt"],
        is("pathspec '*.txt' uses glob or magic syntax; only literal paths are supported"),
    )?;
    assert_refused(
        &root,
        &[b":(top)new.txt"],
        is("pathspec ':(top)new.txt' uses glob or magic syntax; only literal paths are supported"),
    )?;
    for path in [
        &b"../outside"[..],
        b"/abs",
        b"a/../b",
        b".git/config",
        b"dir/.git",
        b"",
    ] {
        assert_refused(&root, &[b"new.txt", path], |error| {
            matches!(error, Error::InvalidPath(_))
        })?;
    }
    Ok(())
}

#[test]
fn add_refuses_ignored_symlinked_nested_and_special_paths() -> Result {
    let temp = tempdir()?;
    let root = committed_repo(&temp)?;
    fs::write(root.join(".gitignore"), b"*.log\nignored/\n")?;
    fs::write(root.join("x.log"), b"log\n")?;
    fs::create_dir(root.join("ignored"))?;
    fs::write(root.join("ignored/file"), b"f\n")?;
    assert_refused(
        &root,
        &[b"new.txt", b"x.log"],
        is("path 'x.log' is ignored by one of your .gitignore files"),
    )?;
    assert_refused(
        &root,
        &[b"ignored"],
        is("path 'ignored' is ignored by one of your .gitignore files"),
    )?;
    assert_refused(
        &root,
        &[b"ignored/file"],
        is("path 'ignored/file' is ignored by one of your .gitignore files"),
    )?;

    git(temp.path(), &["init", "-q", "repo/nested"])?;
    fs::write(root.join("nested/inner"), b"inner\n")?;
    assert_refused(
        &root,
        &[b"nested"],
        is("'nested' is inside or is a nested repository; staging gitlinks is unsupported"),
    )?;
    assert_refused(
        &root,
        &[b"nested/inner"],
        is("'nested' is inside or is a nested repository; staging gitlinks is unsupported"),
    )?;
    assert_refused(
        &root,
        &[b"."],
        is("'nested' is inside or is a nested repository; staging gitlinks is unsupported"),
    )?;
    fs::remove_dir_all(root.join("nested"))?;

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("dir", root.join("alias"))?;
        assert_refused(
            &root,
            &[b"alias/file.txt"],
            is("pathspec 'alias/file.txt' is beyond a symbolic link"),
        )?;
        fs::remove_file(root.join("alias"))?;
        let fifo = root.join("pipe");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()?
                .success()
        );
        assert_refused(
            &root,
            &[b"pipe"],
            is("'pipe': can only add regular files and symbolic links"),
        )?;
        // An untracked FIFO found by a directory walk is skipped, as by Git.
        add(&Repository::discover(&root)?, &[b"."])?;
        assert!(!text(&root, &["ls-files"])?.contains("pipe"));
    }
    Ok(())
}

#[test]
fn add_refuses_paths_that_need_conversion() -> Result {
    let cases: [(&[u8], &str, &[u8]); 6] = [
        (b"*.txt text\n", "", b"a\r\nb\r\n"),
        (b"*.txt text=auto\n", "", b"a\r\nb\r\n"),
        (b"*.txt eol=crlf\n", "", b"a\r\n"),
        (b"*.txt filter=lfs\n", "", b"plain\n"),
        (b"*.txt working-tree-encoding=UTF-16\n", "", b"plain\n"),
        (b"*.txt ident\n", "", b"$Id$\n"),
    ];
    for (attributes, _, content) in cases {
        let temp = tempdir()?;
        let root = committed_repo(&temp)?;
        fs::write(root.join(".gitattributes"), attributes)?;
        fs::write(root.join("new.txt"), content)?;
        assert_refused(&root, &[b"new.txt"], conversion)?;
    }
    // Repository-local core.autocrlf with CRLF content.
    let temp = tempdir()?;
    let root = committed_repo(&temp)?;
    git(&root, &["config", "core.autocrlf", "true"])?;
    fs::write(root.join("new.txt"), b"a\r\n")?;
    assert_refused(&root, &[b"new.txt"], conversion)?;
    // ...and `-text` opts the same content out of conversion.
    fs::write(root.join(".gitattributes"), b"new.txt -text\n")?;
    add(
        &Repository::discover(&root)?,
        &[b"new.txt", b".gitattributes"],
    )?;
    assert_eq!(text(&root, &["cat-file", "-p", ":new.txt"])?, "a");
    Ok(())
}

#[test]
fn add_refuses_unsupported_index_states() -> Result {
    let temp = tempdir()?;
    let root = committed_repo(&temp)?;

    git(&root, &["update-index", "--skip-worktree", "tracked.txt"])?;
    assert_refused(
        &root,
        &[b"tracked.txt"],
        is("'tracked.txt' has the skip-worktree bit; sparse staging is unsupported"),
    )?;
    git(
        &root,
        &["update-index", "--no-skip-worktree", "tracked.txt"],
    )?;
    git(
        &root,
        &["update-index", "--assume-unchanged", "tracked.txt"],
    )?;
    assert_refused(
        &root,
        &[b"."],
        is("'tracked.txt' is marked assume-unchanged; refusing to stage it"),
    )?;
    git(
        &root,
        &["update-index", "--no-assume-unchanged", "tracked.txt"],
    )?;

    git(&root, &["update-index", "--index-version", "4"])?;
    assert_refused(&root, &[b"new.txt"], |error| {
        matches!(error, Error::UnsupportedCapability(_))
    })?;
    git(&root, &["update-index", "--index-version", "2"])?;

    git(&root, &["update-index", "--split-index"])?;
    assert_refused(&root, &[b"new.txt"], |error| {
        matches!(error, Error::UnsupportedCapability(_))
    })?;
    git(&root, &["update-index", "--no-split-index"])?;

    git(&root, &["config", "core.sparseCheckout", "true"])?;
    assert_refused(&root, &[b"new.txt"], |error| {
        matches!(error, Error::UnsupportedCapability(_))
    })?;
    git(&root, &["config", "core.sparseCheckout", "false"])?;

    // A submodule gitlink under the pathspec.
    let sub = temp.path().join("sub");
    git(temp.path(), &["init", "-q", "sub"])?;
    fs::write(sub.join("s"), b"s")?;
    git(&sub, &["add", "s"])?;
    git(&sub, &["commit", "-q", "-m", "s"])?;
    let head = text(&sub, &["rev-parse", "HEAD"])?;
    git(
        &root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{head},module"),
        ],
    )?;
    assert_refused(
        &root,
        &[b"."],
        is("'module' is a submodule (gitlink); staging submodules is unsupported"),
    )?;
    git(&root, &["rm", "-q", "--cached", "module"])?;

    // The repository is still usable for supported adds afterwards.
    add(&Repository::discover(&root)?, &[b"new.txt"])?;
    fsck(&root)?;
    Ok(())
}

#[test]
fn add_refuses_conflicted_paths_and_merges_in_progress() -> Result {
    let temp = tempdir()?;
    let root = committed_repo(&temp)?;
    git(&root, &["checkout", "-q", "-b", "side"])?;
    fs::write(root.join("tracked.txt"), b"side\n")?;
    git(&root, &["commit", "-q", "-am", "side"])?;
    git(&root, &["checkout", "-q", "master"])?;
    fs::write(root.join("tracked.txt"), b"main\n")?;
    git(&root, &["commit", "-q", "-am", "main"])?;
    let merge = git_command(&root).args(["merge", "-q", "side"]).output()?;
    assert!(!merge.status.success());
    assert_refused(&root, &[b"tracked.txt"], |error| {
        matches!(
            error,
            Error::UnsupportedCapability(_) | Error::UnmergedIndex
        )
    })?;
    // Without the merge state, the conflict stages alone refuse the path.
    fs::remove_file(root.join(".git/MERGE_HEAD"))?;
    assert_refused(&root, &[b"tracked.txt"], |error| {
        matches!(error, Error::UnmergedIndex)
    })?;
    assert_refused(&root, &[b"."], |error| {
        matches!(error, Error::UnmergedIndex)
    })?;
    Ok(())
}

#[test]
fn add_refuses_case_only_respellings_on_case_insensitive_repositories() -> Result {
    let temp = tempdir()?;
    let root = committed_repo(&temp)?;
    let ignorecase = text(
        &root,
        &["config", "--bool", "--default", "false", "core.ignorecase"],
    )?;
    if ignorecase != "true" {
        eprintln!("skipping: filesystem is case-sensitive");
        return Ok(());
    }
    assert_refused(
        &root,
        &[b"TRACKED.txt"],
        is("'TRACKED.txt' differs only in case or Unicode normalization from an existing path"),
    )?;
    assert_refused(
        &root,
        &[b"DIR/file.txt"],
        is("'DIR/file.txt' differs only in case or Unicode normalization from an existing path"),
    )?;
    Ok(())
}

#[test]
fn add_refuses_bare_repositories() -> Result {
    let temp = tempdir()?;
    let repo = init(
        &temp.path().join("bare"),
        &options(ObjectFormat::Sha1, true),
    )?;
    assert!(matches!(
        add(&repo, &[b"."]),
        Err(Error::UnsupportedCapability(_))
    ));
    Ok(())
}

#[test]
fn add_with_held_index_lock_is_locked_and_leaves_index_and_lock() -> Result {
    let temp = tempdir()?;
    let root = committed_repo(&temp)?;
    let index = fs::read(root.join(".git/index"))?;
    fs::write(root.join(".git/index.lock"), b"held by another writer")?;
    let result = add(&Repository::discover(&root)?, &[b"new.txt"]);
    assert!(matches!(result, Err(Error::Locked(_))), "{result:?}");
    assert_eq!(fs::read(root.join(".git/index"))?, index);
    assert_eq!(
        fs::read(root.join(".git/index.lock"))?,
        b"held by another writer"
    );
    fs::remove_file(root.join(".git/index.lock"))?;
    // Blobs may already be stored (objects before index); they are harmless.
    fsck(&root)?;
    add(&Repository::discover(&root)?, &[b"new.txt"])?;
    assert!(text(&root, &["ls-files"])?.contains("new.txt"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn init_follows_a_symlinked_worktree_but_refuses_a_symlinked_bare_target() -> Result {
    let temp = tempdir()?;
    let real = temp.path().join("real");
    fs::create_dir(&real)?;
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &link)?;
    init(&link, &InitOptions::default())?;
    assert!(real.join(".git/HEAD").is_file());
    fsck(&real)?;
    let bare_real = temp.path().join("bare-real");
    fs::create_dir(&bare_real)?;
    let bare_link = temp.path().join("bare-link");
    std::os::unix::fs::symlink(&bare_real, &bare_link)?;
    let result = init(&bare_link, &options(ObjectFormat::Sha1, true));
    assert!(
        matches!(result, Err(Error::UnsupportedCapability(_))),
        "{:?}",
        result.err()
    );
    assert!(fs::symlink_metadata(&bare_link)?.file_type().is_symlink());
    assert_eq!(fs::read_dir(&bare_real)?.count(), 0);
    assert_no_init_temporaries(temp.path())?;
    Ok(())
}
