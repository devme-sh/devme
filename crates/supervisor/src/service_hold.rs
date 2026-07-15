//! Reference-counted ownership of Service dependency closures.

use std::collections::{HashMap, HashSet};

use devme_config::{Graph, Stack};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ServiceHoldOwner {
    Task(String),
    Session(String),
    Explicit(String),
}

impl ServiceHoldOwner {
    pub fn task(id: impl Into<String>) -> Self {
        Self::Task(id.into())
    }

    pub fn session(id: impl Into<String>) -> Self {
        Self::Session(id.into())
    }

    pub fn explicit(id: impl Into<String>) -> Self {
        Self::Explicit(id.into())
    }
}

#[derive(Debug)]
pub struct ServiceHolds {
    graph: Graph,
    services: HashSet<String>,
    start_order: Vec<String>,
    stop_order: Vec<String>,
    by_owner: HashMap<ServiceHoldOwner, HashSet<String>>,
}

impl ServiceHolds {
    pub fn new(stack: &Stack) -> Self {
        let graph = Graph::from_stack(stack);
        let start_order = graph
            .topo_sort()
            .unwrap_or_else(|_| stack.service.keys().cloned().collect());
        let mut stop_order = start_order.clone();
        stop_order.reverse();
        Self {
            graph,
            services: stack.service.keys().cloned().collect(),
            start_order,
            stop_order,
            by_owner: HashMap::new(),
        }
    }

    pub fn acquire(&mut self, owner: ServiceHoldOwner, targets: &[String]) -> Vec<String> {
        let before = self.required_set();
        let closure = self.closure(targets);
        self.by_owner.insert(owner, closure);
        let after = self.required_set();
        Self::in_order(&self.start_order, after.difference(&before))
    }

    pub fn release(&mut self, owner: &ServiceHoldOwner) -> Vec<String> {
        let before = self.required_set();
        self.by_owner.remove(owner);
        let after = self.required_set();
        Self::in_order(&self.stop_order, before.difference(&after))
    }

    pub fn required(&self, service: &str) -> bool {
        self.by_owner
            .values()
            .any(|closure| closure.contains(service))
    }

    pub fn contains_owner(&self, owner: &ServiceHoldOwner) -> bool {
        self.by_owner.contains_key(owner)
    }

    fn required_set(&self) -> HashSet<String> {
        self.by_owner
            .values()
            .flat_map(|closure| closure.iter().cloned())
            .collect()
    }

    fn closure(&self, targets: &[String]) -> HashSet<String> {
        fn visit(
            name: &str,
            graph: &Graph,
            services: &HashSet<String>,
            found: &mut HashSet<String>,
        ) {
            if !services.contains(name) || !found.insert(name.to_string()) {
                return;
            }
            for dependency in graph
                .dependencies(name)
                .iter()
                .filter(|dependency| dependency.required)
            {
                visit(&dependency.name, graph, services, found);
            }
        }

        let mut found = HashSet::new();
        for target in targets {
            visit(target, &self.graph, &self.services, &mut found);
        }
        found
    }

    fn in_order<'a>(order: &[String], names: impl Iterator<Item = &'a String>) -> Vec<String> {
        let names = names.cloned().collect::<HashSet<_>>();
        order
            .iter()
            .filter(|name| names.contains(*name))
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use devme_config::Stack;

    use super::{ServiceHoldOwner, ServiceHolds};

    #[test]
    fn releasing_one_owner_preserves_services_required_by_overlapping_holds() {
        let stack = Stack::parse(
            r#"
schema_version = 1
[service.db]
cmd = "sleep 30"
[service.api]
cmd = "sleep 30"
depends_on = ["db"]
[service.logs]
cmd = "sleep 30"
"#,
        )
        .unwrap();
        let mut holds = ServiceHolds::new(&stack);

        holds.acquire(ServiceHoldOwner::task("a"), &["api".into()]);
        holds.acquire(ServiceHoldOwner::task("b"), &["db".into()]);
        holds.acquire(
            ServiceHoldOwner::session("ios"),
            &["api".into(), "logs".into()],
        );

        assert_eq!(
            holds.release(&ServiceHoldOwner::task("a")),
            Vec::<String>::new()
        );
        assert_eq!(
            holds.release(&ServiceHoldOwner::task("b")),
            Vec::<String>::new()
        );
        assert_eq!(
            holds.release(&ServiceHoldOwner::session("ios")),
            vec!["api", "logs", "db"]
        );
    }
}
