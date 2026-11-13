# 0002: Compute ancestry from decoded parent edges

Status: accepted for the first graph-query slice.

Add merge-base (plain, all and is-ancestor) and single-tip rev-list count as
read-only library operations. Reuse Gitoxide's repository/object access and
fallible commit/tag decoding. Build a bounded in-memory graph with interned
object IDs and parent indices.

Kahn's algorithm visits children before parents and detects cycles without
recursion. Propagate a two-bit reachability mask for merge-base inputs. A common
ancestor is a best base exactly when no common descendant has already covered
it. Sort the resulting IDs; plain merge-base selects the first valid base.
Dates and acceleration metadata do not determine traversal order or ancestry.

This prioritizes a simple correctness contract over the optimized paint walks
in C Git or existing Gitoxide merge-base paths. Cost is O(V + E) in the full
reachable graph; there is no early exit. Limits bound retained nodes/links,
not underlying object decoding memory. Optimizations require preserving the
independent oracle and measurement before adoption.

Capture shallow boundaries explicitly and fail on invalid IDs or I/O errors.
Do not silently ignore history-rewriting features: replacement refs and nonempty
graft files are explicitly unsupported for these queries. Refusal is preferable
to answering against different ancestry from the user's repository.

The integration oracle computes brute-force ancestor sets from generated parent
lists, then removes common ancestors dominated by another common ancestor.
Fixtures include the tied-date three-commit regression, criss-cross merges,
disconnected roots and 64 seeded DAGs. Compare both hash formats with loose,
packed, and freshly written generation-v1/v2 commit graphs; verify which graph
format was written. Selected queries also compare CLI output/status with C Git.

Separate tests cover a 20,001-commit chain, graph limits/cycles, shallow history,
malformed/missing/noncommit parents, trusted runtime independence and unchanged
index/ref state. No worktree/index/ref mutations or executable helpers are added.
