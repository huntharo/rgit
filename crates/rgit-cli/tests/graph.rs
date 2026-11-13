use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use rgit_core::{BStr, GraphLimits, ObjectId, Repository};
use tempfile::{TempDir, tempdir};

mod support;
use support::*;

type Parents = Vec<Vec<usize>>;

struct Imported {
    root: TempDir,
    ids: Vec<Vec<ObjectId>>,
}

fn input(root: &Path, args: &[&str], data: &[u8]) -> Result<Output> {
    let mut child = git_command(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().ok_or("missing stdin")?.write_all(data)?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(output)
}

fn import(hash: &str, graphs: &[Parents]) -> Result<Imported> {
    let root = tempdir()?;
    git(
        root.path(),
        &["init", "-b", "main", "--object-format", hash],
    )?;
    let mut stream = String::from("blob\nmark :1\ndata 0\n\n");
    let mut mark = 2;
    let mut graph_marks = Vec::new();
    for (g, graph) in graphs.iter().enumerate() {
        let mut marks = Vec::new();
        for (node, parents) in graph.iter().enumerate() {
            let date = 1_700_000_000 + if g == 0 { 0 } else { (g + node * 17) % 3 };
            let message = format!("graph {g} node {node}");
            stream.push_str(&format!("commit refs/heads/g{g}-c{node}\nmark :{mark}\nauthor Test <test@example.invalid> {date} +0000\ncommitter Test <test@example.invalid> {date} +0000\ndata {}\n{message}\n", message.len()));
            for (i, &parent) in parents.iter().enumerate() {
                stream.push_str(&format!(
                    "{} :{}\n",
                    if i == 0 { "from" } else { "merge" },
                    marks[parent]
                ));
            }
            stream.push_str("M 100644 :1 file\n\n");
            marks.push(mark);
            mark += 1;
        }
        graph_marks.push(marks);
    }
    input(
        root.path(),
        &["fast-import", "--quiet", "--export-marks=marks"],
        stream.as_bytes(),
    )?;
    let marks: HashMap<usize, ObjectId> = fs::read_to_string(root.path().join("marks"))?
        .lines()
        .map(|line| {
            let (mark, hash) = line.split_once(' ').ok_or("invalid mark record")?;
            Ok((
                mark.trim_start_matches(':').parse()?,
                ObjectId::from_hex(hash.as_bytes())?,
            ))
        })
        .collect::<Result<_>>()?;
    let ids = graph_marks
        .iter()
        .map(|graph| {
            graph
                .iter()
                .map(|mark| marks.get(mark).copied().ok_or("missing mark".into()))
                .collect()
        })
        .collect::<Result<_>>()?;
    Ok(Imported { root, ids })
}

fn graphs() -> Vec<Parents> {
    let mut graphs = vec![
        // Minimal tied-date regression: c1, not c0, is the best ancestor.
        vec![vec![], vec![0], vec![0, 1]],
        // Criss-cross: both c1 and c2 must survive as best bases.
        vec![vec![], vec![0], vec![0], vec![1, 2], vec![2, 1]],
        vec![vec![], vec![], vec![0], vec![1]],
    ];
    for seed in 0..64 {
        let count = 3 + seed % 10;
        let mut state = seed as u64 + 1;
        let mut next = || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            state
        };
        let mut graph = Vec::new();
        for node in 0..count {
            let mut parents = Vec::new();
            for parent in 0..node {
                if (next() >> 32) % 4 == 0 {
                    parents.push(parent);
                }
            }
            parents.truncate(3);
            graph.push(parents);
        }
        graphs.push(graph);
    }
    graphs
}

/// Brute-force ancestor sets from generated parent lists, independent of the
/// production graph loading, Kahn traversal, paints and best-base propagation.
fn closure(graph: &Parents) -> Vec<HashSet<usize>> {
    (0..graph.len())
        .map(|start| {
            let mut found = HashSet::new();
            let mut pending = vec![start];
            while let Some(node) = pending.pop() {
                if found.insert(node) {
                    pending.extend(&graph[node]);
                }
            }
            found
        })
        .collect()
}

fn best(ancestors: &[HashSet<usize>], left: usize, right: usize) -> Vec<usize> {
    let common: Vec<_> = ancestors[left]
        .intersection(&ancestors[right])
        .copied()
        .collect();
    common
        .iter()
        .copied()
        .filter(|&candidate| {
            !common
                .iter()
                .any(|&other| other != candidate && ancestors[other].contains(&candidate))
        })
        .collect()
}

fn lines(output: &[u8]) -> Result<BTreeSet<String>> {
    Ok(std::str::from_utf8(output)?
        .lines()
        .map(str::to_owned)
        .collect())
}

#[test]
fn seeded_dags_match_independent_oracle_with_both_hashes_and_graph_formats() -> Result {
    let graphs = graphs();
    let mut queries = 0;
    for hash in ["sha1", "sha256"] {
        let fixture = import(hash, &graphs)?;
        for mode in 0..4 {
            if mode == 1 {
                git(fixture.root.path(), &["gc", "--prune=now"])?;
            }
            if mode >= 2 {
                let graph_path = fixture.root.path().join(".git/objects/info/commit-graph");
                if graph_path.exists() {
                    fs::remove_file(&graph_path)?;
                }
                git(
                    fixture.root.path(),
                    &[
                        "-c",
                        &format!("commitGraph.generationVersion={}", mode - 1),
                        "commit-graph",
                        "write",
                        "--reachable",
                    ],
                )?;
                let bytes = fs::read(graph_path)?;
                assert!(bytes.starts_with(b"CGPH"));
                assert_eq!(bytes.windows(4).any(|chunk| chunk == b"GDA2"), mode == 3);
            }
            let repo = Repository::discover(fixture.root.path())?;
            for (g, graph) in graphs.iter().enumerate() {
                let ancestors = closure(graph);
                let ids = &fixture.ids[g];
                let pairs = [
                    (0, ids.len() - 1),
                    (ids.len() - 2, ids.len() - 1),
                    (ids.len() / 2, ids.len() - 1),
                ];
                for (left, right) in pairs {
                    let left_id = ids[left].to_string();
                    let right_id = ids[right].to_string();
                    let expected: BTreeSet<_> = best(&ancestors, left, right)
                        .iter()
                        .map(|&index| ids[index].to_string())
                        .collect();
                    let actual: BTreeSet<_> = repo
                        .merge_bases(
                            BStr::new(&left_id),
                            BStr::new(&right_id),
                            GraphLimits::default(),
                        )?
                        .into_iter()
                        .map(|id| id.to_string())
                        .collect();
                    assert_eq!(
                        actual, expected,
                        "{hash} mode={mode} graph={g} pair={left}/{right}"
                    );
                    assert_eq!(
                        repo.is_ancestor(
                            BStr::new(&left_id),
                            BStr::new(&right_id),
                            GraphLimits::default()
                        )?,
                        ancestors[right].contains(&left)
                    );
                    assert_eq!(
                        repo.ancestor_count(BStr::new(&right_id), GraphLimits::default())?,
                        ancestors[right].len()
                    );
                    queries += 1;
                    if g < 3 && mode == 0 {
                        let actual = rgit(
                            fixture.root.path(),
                            &["merge-base", "--all", &left_id, &right_id],
                        )?;
                        assert_eq!(actual.status.code(), Some(i32::from(expected.is_empty())));
                        assert_eq!(lines(&actual.stdout)?, expected);
                        let plain =
                            rgit(fixture.root.path(), &["merge-base", &left_id, &right_id])?;
                        if expected.is_empty() {
                            assert_eq!(plain.status.code(), Some(1));
                        } else {
                            assert!(expected.contains(std::str::from_utf8(&plain.stdout)?.trim()));
                        }
                        let c_git = git_command(fixture.root.path())
                            .args(["merge-base", "--all", &left_id, &right_id])
                            .output()?;
                        assert_eq!(c_git.status.code(), actual.status.code());
                        assert_eq!(lines(&c_git.stdout)?, expected);
                        same(
                            fixture.root.path(),
                            &["merge-base", "--is-ancestor", &left_id, &right_id],
                        )?;
                        same(fixture.root.path(), &["rev-list", "--count", &right_id])?;
                    }
                }
            }
        }
    }
    assert_eq!(queries, 1608);
    Ok(())
}

#[test]
fn tag_inputs_identical_commits_and_cli_errors() -> Result {
    let root = fixture("sha1")?;
    same(root.path(), &["merge-base", "HEAD", "v1"])?;
    same(root.path(), &["merge-base", "--is-ancestor", "v1", "HEAD"])?;
    same(root.path(), &["rev-list", "--count", "v1"])?;
    for args in [
        vec!["merge-base", "HEAD", "HEAD:file.txt"],
        vec!["merge-base", "--all", "--is-ancestor", "HEAD", "HEAD"],
        vec!["merge-base", "--octopus", "HEAD", "HEAD"],
        vec!["rev-list", "HEAD"],
        vec!["rev-list", "--count", "HEAD", "HEAD"],
    ] {
        let output = rgit(root.path(), &args)?;
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
    Ok(())
}

#[test]
fn shallow_boundaries_cut_existing_and_missing_parent_history() -> Result {
    for hash in ["sha1", "sha256"] {
        let fixture = import(hash, &[vec![vec![], vec![0], vec![1]]])?;
        let ids = &fixture.ids[0];
        fs::write(
            fixture.root.path().join(".git/shallow"),
            format!("{}\n", ids[1]),
        )?;
        let a = ids[0].to_string();
        let b = ids[1].to_string();
        let c = ids[2].to_string();
        same(fixture.root.path(), &["rev-list", "--count", &c])?;
        same(
            fixture.root.path(),
            &["merge-base", "--is-ancestor", &a, &c],
        )?;
        same(fixture.root.path(), &["merge-base", "--all", &b, &c])?;
        let hex = ids[0].to_string();
        fs::remove_file(
            fixture
                .root
                .path()
                .join(".git/objects")
                .join(&hex[..2])
                .join(&hex[2..]),
        )?;
        same(fixture.root.path(), &["rev-list", "--count", &c])?;
        fs::write(
            fixture.root.path().join(".git/shallow"),
            b"not-an-object-id\n",
        )?;
        let output = rgit(fixture.root.path(), &["rev-list", "--count", &c])?;
        assert_eq!(output.status.code(), Some(128));
        assert!(output.stdout.is_empty());
    }
    Ok(())
}

#[test]
fn malformed_missing_and_noncommit_parents_are_errors_without_partial_output() -> Result {
    let root = fixture("sha1")?;
    let tree = String::from_utf8(git(root.path(), &["rev-parse", "HEAD^{tree}"])?.stdout)?;
    let blob = String::from_utf8(git(root.path(), &["rev-parse", "HEAD:file.txt"])?.stdout)?;
    for data in [
        "not a commit\n".to_owned(),
        format!(
            "tree {}\nparent {}\nauthor T <t@example.invalid> 1 +0000\ncommitter T <t@example.invalid> 1 +0000\n\nbad parent\n",
            tree.trim(),
            "1".repeat(40)
        ),
        format!(
            "tree {}\nparent {}\nauthor T <t@example.invalid> 1 +0000\ncommitter T <t@example.invalid> 1 +0000\n\nblob parent\n",
            tree.trim(),
            blob.trim()
        ),
    ] {
        let output = input(
            root.path(),
            &[
                "hash-object",
                "-w",
                "--stdin",
                "--literally",
                "-t",
                "commit",
            ],
            data.as_bytes(),
        )?;
        let id = String::from_utf8(output.stdout)?;
        for args in [
            vec!["rev-list", "--count", id.trim()],
            vec!["merge-base", "--all", "HEAD", id.trim()],
        ] {
            let output = rgit(root.path(), &args)?;
            assert_eq!(output.status.code(), Some(128));
            assert!(output.stdout.is_empty());
            assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
        }
    }
    Ok(())
}

#[test]
fn replacement_refs_and_grafts_are_explicitly_unsupported() -> Result {
    let root = fixture("sha1")?;
    fs::write(root.path().join(".git/info/grafts"), b"historical graft\n")?;
    let output = rgit(root.path(), &["rev-list", "--count", "HEAD"])?;
    assert_eq!(output.status.code(), Some(128));
    assert!(String::from_utf8_lossy(&output.stderr).contains("grafts"));
    fs::remove_file(root.path().join(".git/info/grafts"))?;
    let id = String::from_utf8(git(root.path(), &["rev-parse", "HEAD"])?.stdout)?;
    git(
        root.path(),
        &[
            "update-ref",
            &format!("refs/replace/{}", id.trim()),
            id.trim(),
        ],
    )?;
    let output = rgit(root.path(), &["rev-list", "--count", "HEAD"])?;
    assert_eq!(output.status.code(), Some(128));
    assert!(String::from_utf8_lossy(&output.stderr).contains("replacement"));
    Ok(())
}

#[test]
fn graph_queries_preserve_repository_state_and_need_no_helpers() -> Result {
    let root = fixture("sha1")?;
    let index = fs::read(root.path().join(".git/index"))?;
    let refs = git(root.path(), &["show-ref"])?.stdout;
    let empty = tempdir()?;
    let output = Command::new(env!("CARGO_BIN_EXE_rgit"))
        .arg("-C")
        .arg(root.path())
        .args(["merge-base", "--all", "v1", "HEAD"])
        .env("PATH", empty.path())
        .output()?;
    assert!(output.status.success());
    assert_eq!(fs::read(root.path().join(".git/index"))?, index);
    assert_eq!(git(root.path(), &["show-ref"])?.stdout, refs);
    Ok(())
}
