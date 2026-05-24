//! Dependency graph built from a validated [`Stack`].
//!
//! [`crate::validate`] already proves the config has no cycles; this module
//! lets the supervisor consume that structure: walk it in topological order,
//! ask whether a node's dependencies are satisfied given current state, and
//! enumerate parallel-startable layers for display.

use std::collections::{HashMap, HashSet, VecDeque};

use devme_core::Dependency;
use thiserror::Error;

use crate::stack::Stack;

/// Distinguishes the two kinds of nodes the supervisor handles differently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NodeKind {
    /// A Step: setup task with a `check` (and optional `provision`).
    Step,
    /// A Service: long-running process.
    Service,
}

#[derive(Debug, Clone)]
pub struct Graph {
    /// Forward edges: node -> its declared dependencies.
    edges: HashMap<String, Vec<Dependency>>,
    /// Insertion order matches declaration order — used for stable output.
    nodes: Vec<String>,
    /// What kind of node each name refers to.
    kinds: HashMap<String, NodeKind>,
    /// Names of steps that declare a `provision` command.
    has_provision: std::collections::HashSet<String>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum GraphError {
    #[error("dependency cycle involving {0:?}")]
    Cycle(Vec<String>),
}

/// Outcome of asking "is this dependency satisfied?", as understood by the
/// caller (the executor). Optional deps that aren't `Satisfied` don't block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepStatus {
    Satisfied,
    Pending,
    Failed,
}

impl Graph {
    pub fn from_stack(stack: &Stack) -> Self {
        let mut edges = HashMap::new();
        let mut nodes = Vec::with_capacity(stack.step.len() + stack.service.len());
        let mut kinds = HashMap::new();
        let mut has_provision = std::collections::HashSet::new();

        for (name, step) in &stack.step {
            edges.insert(name.clone(), step.depends_on.clone());
            kinds.insert(name.clone(), NodeKind::Step);
            if step.provision.is_some() {
                has_provision.insert(name.clone());
            }
            nodes.push(name.clone());
        }
        for (name, service) in &stack.service {
            edges.insert(name.clone(), service.depends_on.clone());
            kinds.insert(name.clone(), NodeKind::Service);
            nodes.push(name.clone());
        }

        Self { edges, nodes, kinds, has_provision }
    }

    /// True if `node` is a Step whose config declared a `provision`.
    pub fn has_provision(&self, node: &str) -> bool {
        self.has_provision.contains(node)
    }

    pub fn nodes(&self) -> &[String] {
        &self.nodes
    }

    pub fn kind(&self, node: &str) -> Option<NodeKind> {
        self.kinds.get(node).copied()
    }

    pub fn dependencies(&self, node: &str) -> &[Dependency] {
        self.edges
            .get(node)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Topological order: every node appears after all of its required
    /// dependencies. Within a layer (no ordering constraint), declaration
    /// order is preserved so output is deterministic.
    pub fn topo_sort(&self) -> Result<Vec<String>, GraphError> {
        let mut out = Vec::with_capacity(self.nodes.len());
        for layer in self.layers()? {
            out.extend(layer);
        }
        Ok(out)
    }

    /// Layered ordering: each `Vec` is a set of nodes whose dependencies are
    /// all in earlier layers. Useful for parallel-startable groups and for
    /// the TUI "plan" preview.
    pub fn layers(&self) -> Result<Vec<Vec<String>>, GraphError> {
        // Kahn's algorithm with grouped output. Count predecessors using only
        // *required* edges — optional deps don't block startup ordering.
        let mut in_degree: HashMap<&str, usize> = self
            .nodes
            .iter()
            .map(|n| (n.as_str(), 0))
            .collect();
        let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();

        for (node, deps) in &self.edges {
            for dep in deps {
                if !dep.required {
                    continue;
                }
                if in_degree.contains_key(dep.name.as_str()) {
                    *in_degree.get_mut(node.as_str()).unwrap() += 1;
                    reverse
                        .entry(dep.name.as_str())
                        .or_default()
                        .push(node.as_str());
                }
            }
        }

        let mut layers = Vec::new();
        let mut current: Vec<&str> = self
            .nodes
            .iter()
            .filter(|n| in_degree[n.as_str()] == 0)
            .map(String::as_str)
            .collect();

        let mut visited = 0;
        while !current.is_empty() {
            visited += current.len();
            let mut next: Vec<&str> = Vec::new();
            for node in &current {
                if let Some(succs) = reverse.get(node) {
                    for succ in succs {
                        let d = in_degree.get_mut(succ).unwrap();
                        *d -= 1;
                        if *d == 0 {
                            next.push(succ);
                        }
                    }
                }
            }
            // Sort `next` by declaration order so layers are deterministic.
            let order: HashMap<&str, usize> = self
                .nodes
                .iter()
                .enumerate()
                .map(|(i, n)| (n.as_str(), i))
                .collect();
            next.sort_by_key(|n| order[n]);

            layers.push(current.iter().map(|s| s.to_string()).collect());
            current = next;
        }

        if visited != self.nodes.len() {
            // Surface the unresolved nodes — validate() catches cycles too,
            // but defensive callers may hit this without first validating.
            let unresolved: Vec<String> = self
                .nodes
                .iter()
                .filter(|n| in_degree[n.as_str()] > 0)
                .cloned()
                .collect();
            return Err(GraphError::Cycle(unresolved));
        }

        Ok(layers)
    }

    /// Are all required dependencies of `node` satisfied? Optional deps
    /// don't block: a `Pending` optional is treated as "go ahead", and a
    /// `Failed` optional is logged at the executor level, not here.
    pub fn deps_satisfied(
        &self,
        node: &str,
        status: impl Fn(&str) -> DepStatus,
    ) -> SatisfactionOutcome {
        let mut all_required_ok = true;
        let mut any_failed = false;
        for dep in self.dependencies(node) {
            let s = status(&dep.name);
            match (dep.required, s) {
                (true, DepStatus::Satisfied) => {}
                (true, DepStatus::Pending) => all_required_ok = false,
                (true, DepStatus::Failed) => any_failed = true,
                (false, _) => {}
            }
        }

        if any_failed {
            SatisfactionOutcome::DependencyFailed
        } else if all_required_ok {
            SatisfactionOutcome::Ready
        } else {
            SatisfactionOutcome::Waiting
        }
    }

    /// All nodes that depend (directly or transitively) on `node`.
    pub fn descendants(&self, node: &str) -> Vec<String> {
        let mut reverse: HashMap<&str, Vec<&str>> = HashMap::new();
        for (from, deps) in &self.edges {
            for dep in deps {
                reverse
                    .entry(dep.name.as_str())
                    .or_default()
                    .push(from.as_str());
            }
        }

        let mut seen = HashSet::new();
        let mut queue = VecDeque::new();
        queue.push_back(node);
        while let Some(n) = queue.pop_front() {
            if let Some(succs) = reverse.get(n) {
                for s in succs {
                    if seen.insert(*s) {
                        queue.push_back(s);
                    }
                }
            }
        }

        // Return in declaration order for determinism.
        self.nodes
            .iter()
            .filter(|n| seen.contains(n.as_str()))
            .cloned()
            .collect()
    }
}

/// What `deps_satisfied` decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SatisfactionOutcome {
    /// All required deps are `Satisfied`; node can advance.
    Ready,
    /// At least one required dep is still `Pending`; wait.
    Waiting,
    /// At least one required dep is `Failed`; this node will not start
    /// unless the user explicitly overrides.
    DependencyFailed,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml_str: &str) -> Stack {
        Stack::parse(toml_str).expect("parse")
    }

    #[test]
    fn empty_stack_has_empty_graph() {
        let g = Graph::from_stack(&parse("schema_version = 1"));
        assert!(g.nodes().is_empty());
        assert_eq!(g.topo_sort().unwrap(), Vec::<String>::new());
        assert_eq!(g.layers().unwrap(), Vec::<Vec<String>>::new());
    }

    #[test]
    fn single_node_is_one_layer() {
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"
"#,
        ));
        assert_eq!(g.layers().unwrap(), vec![vec!["a".to_string()]]);
    }

    #[test]
    fn linear_chain_topo_order() {
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.c]
cmd = "true"
depends_on = ["b"]

[service.b]
cmd = "true"
depends_on = ["a"]

[service.a]
cmd = "true"
"#,
        ));
        assert_eq!(g.topo_sort().unwrap(), vec!["a", "b", "c"]);
    }

    #[test]
    fn diamond_produces_three_layers() {
        // a -> {b, c} -> d
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"

[service.b]
cmd = "true"
depends_on = ["a"]

[service.c]
cmd = "true"
depends_on = ["a"]

[service.d]
cmd = "true"
depends_on = ["b", "c"]
"#,
        ));
        assert_eq!(
            g.layers().unwrap(),
            vec![
                vec!["a".to_string()],
                vec!["b".to_string(), "c".to_string()],
                vec!["d".to_string()],
            ]
        );
    }

    #[test]
    fn steps_and_services_share_the_graph() {
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[step.tools]
check = "true"

[service.backend]
cmd = "true"
depends_on = ["tools"]
"#,
        ));
        assert_eq!(g.topo_sort().unwrap(), vec!["tools", "backend"]);
    }

    #[test]
    fn optional_deps_do_not_constrain_order() {
        // backend depends optionally on slow_service; slow_service can come
        // after backend in topo order without breaking.
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.backend]
cmd = "true"
depends_on = ["slow_service?"]

[service.slow_service]
cmd = "true"
"#,
        ));
        let order = g.topo_sort().unwrap();
        // Both should appear; relative order isn't constrained.
        assert!(order.contains(&"backend".to_string()));
        assert!(order.contains(&"slow_service".to_string()));
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn unknown_required_deps_do_not_create_cycles() {
        // validate() should reject unknown deps; here we make sure layers()
        // doesn't itself fall over (it filters unknown targets out).
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"
depends_on = ["nowhere"]
"#,
        ));
        assert_eq!(g.topo_sort().unwrap(), vec!["a"]);
    }

    #[test]
    fn cycle_is_reported() {
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"
depends_on = ["b"]

[service.b]
cmd = "true"
depends_on = ["a"]
"#,
        ));
        let err = g.layers().unwrap_err();
        let GraphError::Cycle(nodes) = err;
        assert_eq!(nodes.len(), 2);
    }

    #[test]
    fn deps_satisfied_ready_when_all_satisfied() {
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"

[service.b]
cmd = "true"
depends_on = ["a"]
"#,
        ));
        let out = g.deps_satisfied("b", |_| DepStatus::Satisfied);
        assert_eq!(out, SatisfactionOutcome::Ready);
    }

    #[test]
    fn deps_satisfied_waiting_when_required_pending() {
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"

[service.b]
cmd = "true"
depends_on = ["a"]
"#,
        ));
        let out = g.deps_satisfied("b", |_| DepStatus::Pending);
        assert_eq!(out, SatisfactionOutcome::Waiting);
    }

    #[test]
    fn deps_satisfied_fails_when_required_failed() {
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"

[service.b]
cmd = "true"
depends_on = ["a"]
"#,
        ));
        let out = g.deps_satisfied("b", |_| DepStatus::Failed);
        assert_eq!(out, SatisfactionOutcome::DependencyFailed);
    }

    #[test]
    fn deps_satisfied_ignores_optional_pending() {
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"

[service.b]
cmd = "true"
depends_on = ["a?"]
"#,
        ));
        let out = g.deps_satisfied("b", |_| DepStatus::Pending);
        assert_eq!(out, SatisfactionOutcome::Ready);
    }

    #[test]
    fn deps_satisfied_ignores_optional_failed() {
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"

[service.b]
cmd = "true"
depends_on = ["a?"]
"#,
        ));
        let out = g.deps_satisfied("b", |_| DepStatus::Failed);
        assert_eq!(out, SatisfactionOutcome::Ready);
    }

    #[test]
    fn descendants_returns_transitive_dependents() {
        // a <- b <- c, a <- d
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"

[service.b]
cmd = "true"
depends_on = ["a"]

[service.c]
cmd = "true"
depends_on = ["b"]

[service.d]
cmd = "true"
depends_on = ["a"]
"#,
        ));
        let mut descs = g.descendants("a");
        descs.sort();
        assert_eq!(descs, vec!["b", "c", "d"]);
    }

    #[test]
    fn descendants_of_leaf_is_empty() {
        let g = Graph::from_stack(&parse(
            r#"
schema_version = 1

[service.a]
cmd = "true"

[service.b]
cmd = "true"
depends_on = ["a"]
"#,
        ));
        assert!(g.descendants("b").is_empty());
    }
}
