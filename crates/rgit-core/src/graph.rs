//! Ancestry comes only from parent edges, never dates or generation metadata.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;

use gix::bstr::ByteSlice;

use crate::{BStr, Error, ObjectId, ObjectKind, Repository};

/// Per-query bounds on retained graph nodes and parent links.
/// These are not bounds on Gitoxide's object/pack decompression memory.
#[derive(Debug, Clone, Copy)]
pub struct GraphLimits {
    pub max_commits: usize,
    pub max_parent_links: usize,
}

impl Default for GraphLimits {
    fn default() -> Self {
        Self {
            max_commits: 1_000_000,
            max_parent_links: 4_000_000,
        }
    }
}

struct Node {
    id: ObjectId,
    parents: Vec<usize>,
}

struct Graph {
    nodes: Vec<Node>,
    roots: Vec<usize>,
    /// Children precede parents; computed without recursion or timestamps.
    order: Vec<usize>,
}

impl Repository {
    /// All best common ancestors, sorted by object ID. An empty set is valid.
    pub fn merge_bases(
        &self,
        left: &BStr,
        right: &BStr,
        limits: GraphLimits,
    ) -> Result<Vec<ObjectId>, Error> {
        let roots = [self.commit_id(left)?, self.commit_id(right)?];
        let graph = self.commit_graph(&roots, limits)?;
        Ok(graph.merge_bases())
    }

    pub fn is_ancestor(
        &self,
        ancestor: &BStr,
        descendant: &BStr,
        limits: GraphLimits,
    ) -> Result<bool, Error> {
        let roots = [self.commit_id(ancestor)?, self.commit_id(descendant)?];
        let graph = self.commit_graph(&roots, limits)?;
        Ok(graph.merge_bases().contains(&roots[0]))
    }

    /// Count unique reachable commits, treating shallow commits as roots.
    pub fn ancestor_count(&self, revision: &BStr, limits: GraphLimits) -> Result<usize, Error> {
        let root = self.commit_id(revision)?;
        Ok(self.commit_graph(&[root], limits)?.nodes.len())
    }

    fn commit_id(&self, revision: &BStr) -> Result<ObjectId, Error> {
        let mut id = self.resolve(revision)?;
        for _ in 0..64 {
            let object = self.inner.find_object(id)?;
            match object.kind {
                ObjectKind::Commit => return Ok(id),
                ObjectKind::Tag => id = object.try_to_tag_ref()?.target(),
                actual => {
                    return Err(Error::UnexpectedKind {
                        expected: ObjectKind::Commit,
                        actual,
                    });
                }
            }
        }
        Err(Error::PeelLimit)
    }

    fn commit_graph(&self, roots: &[ObjectId], limits: GraphLimits) -> Result<Graph, Error> {
        let references = self.inner.references()?;
        if let Some(reference) = references.prefixed("refs/replace/")?.next() {
            reference?;
            return Err(Error::UnsupportedHistory("replacement refs"));
        }
        match std::fs::metadata(self.inner.common_dir().join("info/grafts")) {
            Ok(metadata) if metadata.len() > 0 => {
                return Err(Error::UnsupportedHistory("commit grafts"));
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
        let shallow = self.shallow_boundaries(limits)?;
        Graph::load(roots, limits, |id| {
            let object = self.inner.find_object(id)?;
            let commit = object.try_to_commit_ref()?;
            if shallow.contains(&id) {
                Ok(Vec::new())
            } else {
                Ok(commit.parents().collect())
            }
        })
    }

    fn shallow_boundaries(&self, limits: GraphLimits) -> Result<HashSet<ObjectId>, Error> {
        let file = match std::fs::File::open(self.inner.shallow_file()?) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashSet::new()),
            Err(error) => return Err(error.into()),
        };
        // Enough for one SHA-256 ID plus CRLF per permitted record. Reading
        // through a capped handle also bounds a concurrently growing file.
        let max_bytes = limits.max_commits.saturating_mul(66);
        let mut bytes = Vec::new();
        file.take(
            u64::try_from(max_bytes)
                .unwrap_or(u64::MAX)
                .saturating_add(1),
        )
        .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            return Err(Error::GraphLimit {
                resource: "shallow bytes",
                limit: max_bytes,
            });
        }
        let mut result = HashSet::new();
        for (index, line) in bytes.lines().enumerate() {
            if index >= limits.max_commits {
                return Err(Error::GraphLimit {
                    resource: "shallow records",
                    limit: limits.max_commits,
                });
            }
            let id = ObjectId::from_hex(line).map_err(|_| Error::InvalidShallow)?;
            if id.kind() != self.inner.object_hash() {
                return Err(Error::InvalidShallow);
            }
            result.insert(id);
        }
        Ok(result)
    }
}

impl Graph {
    fn load(
        roots: &[ObjectId],
        limits: GraphLimits,
        mut parents: impl FnMut(ObjectId) -> Result<Vec<ObjectId>, Error>,
    ) -> Result<Self, Error> {
        let mut nodes = Vec::new();
        let mut indices = HashMap::new();
        let mut root_indices = Vec::new();
        for &id in roots {
            root_indices.push(Self::intern(id, limits, &mut nodes, &mut indices)?);
        }
        let mut links = 0usize;
        let mut next = 0;
        while next < nodes.len() {
            let parent_ids = parents(nodes[next].id)?;
            if parent_ids.len() > limits.max_parent_links.saturating_sub(links) {
                return Err(Error::GraphLimit {
                    resource: "parent links",
                    limit: limits.max_parent_links,
                });
            }
            links += parent_ids.len();
            for id in parent_ids {
                let parent = Self::intern(id, limits, &mut nodes, &mut indices)?;
                nodes[next].parents.push(parent);
            }
            next += 1;
        }
        let mut children = vec![0usize; nodes.len()];
        for node in &nodes {
            for &parent in &node.parents {
                children[parent] += 1;
            }
        }
        let mut ready: VecDeque<_> = children
            .iter()
            .enumerate()
            .filter_map(|(index, &count)| (count == 0).then_some(index))
            .collect();
        let mut order = Vec::with_capacity(nodes.len());
        while let Some(index) = ready.pop_front() {
            order.push(index);
            for &parent in &nodes[index].parents {
                children[parent] -= 1;
                if children[parent] == 0 {
                    ready.push_back(parent);
                }
            }
        }
        if order.len() != nodes.len() {
            return Err(Error::GraphCycle);
        }
        Ok(Self {
            nodes,
            roots: root_indices,
            order,
        })
    }

    fn intern(
        id: ObjectId,
        limits: GraphLimits,
        nodes: &mut Vec<Node>,
        indices: &mut HashMap<ObjectId, usize>,
    ) -> Result<usize, Error> {
        if let Some(&index) = indices.get(&id) {
            return Ok(index);
        }
        if nodes.len() >= limits.max_commits {
            return Err(Error::GraphLimit {
                resource: "commits",
                limit: limits.max_commits,
            });
        }
        let index = nodes.len();
        nodes.push(Node {
            id,
            parents: Vec::new(),
        });
        indices.insert(id, index);
        Ok(index)
    }

    fn merge_bases(&self) -> Vec<ObjectId> {
        let mut paint = vec![0u8; self.nodes.len()];
        for (side, &index) in self.roots.iter().enumerate() {
            paint[index] |= 1 << side;
        }
        let mut covered = vec![false; self.nodes.len()];
        let mut result = Vec::new();
        for &index in &self.order {
            let common = paint[index] == 3;
            if common && !covered[index] {
                result.push(self.nodes[index].id);
            }
            for &parent in &self.nodes[index].parents {
                paint[parent] |= paint[index];
                covered[parent] |= common || covered[index];
            }
        }
        result.sort();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(value: u8) -> Result<ObjectId, Error> {
        ObjectId::from_hex(format!("{value:040x}").as_bytes()).map_err(|_| Error::InvalidShallow)
    }

    #[test]
    fn graph_limits_and_cycles_are_errors() -> Result<(), Error> {
        let a = id(1)?;
        let b = id(2)?;
        let graph = |limits| {
            Graph::load(&[a], limits, |node| {
                Ok(if node == a { vec![b] } else { vec![] })
            })
        };
        assert!(matches!(
            graph(GraphLimits {
                max_commits: 1,
                max_parent_links: 4
            }),
            Err(Error::GraphLimit {
                resource: "commits",
                ..
            })
        ));
        assert!(matches!(
            graph(GraphLimits {
                max_commits: 4,
                max_parent_links: 0
            }),
            Err(Error::GraphLimit {
                resource: "parent links",
                ..
            })
        ));
        assert!(matches!(
            Graph::load(&[a], GraphLimits::default(), |_| Ok(vec![a])),
            Err(Error::GraphCycle)
        ));
        Ok(())
    }

    #[test]
    fn deep_history_uses_no_recursion() -> Result<(), Error> {
        let depth = 20_000u32;
        let last = ObjectId::from_hex(format!("{depth:040x}").as_bytes())
            .map_err(|_| Error::InvalidShallow)?;
        let graph = Graph::load(&[last], GraphLimits::default(), |node| {
            let n = u32::from_str_radix(&node.to_string()[32..], 16)
                .map_err(|_| Error::InvalidShallow)?;
            if n == 0 {
                Ok(vec![])
            } else {
                Ok(vec![
                    ObjectId::from_hex(format!("{:040x}", n - 1).as_bytes())
                        .map_err(|_| Error::InvalidShallow)?,
                ])
            }
        })?;
        assert_eq!(graph.nodes.len(), depth as usize + 1);
        Ok(())
    }
}
