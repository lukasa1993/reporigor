use std::collections::{BTreeMap, BTreeSet};

use reporigor_core::{DependencyRecord, DependencyScope, PackageRecord};

use crate::count_as_f64;

/// Count workspace packages with a direct production edge to each package.
#[must_use]
pub fn afferent_coupling(
    packages: &[PackageRecord],
    dependencies: &[DependencyRecord],
) -> BTreeMap<String, usize> {
    let package_names: BTreeSet<_> = packages.iter().map(|package| package.name.as_str()).collect();
    let mut incoming = empty_coupling_map(packages);
    for edge in production_edges(dependencies) {
        if edge.internal && package_names.contains(edge.dependency.as_str()) {
            if let Some(sources) = incoming.get_mut(&edge.dependency) {
                sources.insert(edge.package.clone());
            }
        }
    }
    coupling_sizes(incoming)
}

/// Count distinct direct production dependencies, internal and external.
#[must_use]
pub fn efferent_coupling(
    packages: &[PackageRecord],
    dependencies: &[DependencyRecord],
) -> BTreeMap<String, usize> {
    let mut outgoing = empty_coupling_map(packages);
    for edge in production_edges(dependencies) {
        if let Some(targets) = outgoing.get_mut(&edge.package) {
            targets.insert(edge.dependency.clone());
        }
    }
    coupling_sizes(outgoing)
}

fn empty_coupling_map(packages: &[PackageRecord]) -> BTreeMap<String, BTreeSet<String>> {
    packages
        .iter()
        .map(|package| (package.name.clone(), BTreeSet::new()))
        .collect()
}

fn coupling_sizes(couplings: BTreeMap<String, BTreeSet<String>>) -> BTreeMap<String, usize> {
    couplings
        .into_iter()
        .map(|(package, targets)| (package, targets.len()))
        .collect()
}

/// Robert Martin instability: `Ce / (Ca + Ce)`, or zero for an isolated node.
#[must_use]
pub fn instability(afferent: usize, efferent: usize) -> f64 {
    let total = afferent.saturating_add(efferent);
    if total == 0 {
        0.0
    } else {
        count_as_f64(efferent) / count_as_f64(total)
    }
}

/// Return canonical strongly connected components containing a cycle.
#[must_use]
pub fn dependency_cycles(packages: &[PackageRecord], dependencies: &[DependencyRecord]) -> Vec<Vec<String>> {
    let graph = internal_dependency_graph(packages, dependencies);
    let components = strongly_connected_components(&graph);
    canonical_cycles(&graph, components)
}

fn internal_dependency_graph(
    packages: &[PackageRecord],
    dependencies: &[DependencyRecord],
) -> BTreeMap<String, BTreeSet<String>> {
    let names: BTreeSet<_> = packages.iter().map(|package| package.name.clone()).collect();
    let mut graph: BTreeMap<String, BTreeSet<String>> =
        names.iter().map(|name| (name.clone(), BTreeSet::new())).collect();
    for edge in production_edges(dependencies) {
        if edge.internal && names.contains(&edge.package) && names.contains(&edge.dependency) {
            graph
                .entry(edge.package.clone())
                .or_default()
                .insert(edge.dependency.clone());
        }
    }
    graph
}

fn strongly_connected_components(graph: &BTreeMap<String, BTreeSet<String>>) -> Vec<Vec<String>> {
    let mut tarjan = Tarjan::new(graph);
    for node in graph.keys() {
        if !tarjan.indices.contains_key(node) {
            tarjan.visit(node);
        }
    }
    tarjan.components
}

fn canonical_cycles(
    graph: &BTreeMap<String, BTreeSet<String>>,
    components: Vec<Vec<String>>,
) -> Vec<Vec<String>> {
    let mut cycles: Vec<_> = components
        .into_iter()
        .filter(|component| component_is_cycle(graph, component))
        .collect();
    for cycle in &mut cycles {
        cycle.sort();
    }
    cycles.sort();
    cycles
}

fn component_is_cycle(graph: &BTreeMap<String, BTreeSet<String>>, component: &[String]) -> bool {
    if component.len() > 1 {
        return true;
    }
    component
        .first()
        .is_some_and(|node| node_has_self_edge(graph, node))
}

fn node_has_self_edge(graph: &BTreeMap<String, BTreeSet<String>>, node: &str) -> bool {
    graph.get(node).is_some_and(|edges| edges.contains(node))
}

fn production_edges(dependencies: &[DependencyRecord]) -> impl Iterator<Item = &DependencyRecord> {
    dependencies
        .iter()
        .filter(|edge| edge.scope == DependencyScope::Production && !edge.target_gated)
}

/// Match one deterministic `*` wildcard pattern.
#[must_use]
pub fn matches_pattern(pattern: &str, value: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == value,
        Some((prefix, suffix)) => {
            !suffix.contains('*')
                && value.len() >= prefix.len().saturating_add(suffix.len())
                && value.starts_with(prefix)
                && value.ends_with(suffix)
        }
    }
}

/// Parse a configured `source->target` forbidden edge predicate.
///
/// # Errors
///
/// Returns an error unless the predicate is a canonical, non-empty
/// `source->target` pair with at most one wildcard on either side.
pub fn parse_edge_pattern(pattern: &str) -> Result<(&str, &str), String> {
    let (source, target) = pattern
        .split_once("->")
        .ok_or_else(|| format!("invalid dependency edge {pattern:?}; expected source->target"))?;
    if edge_pattern_is_invalid(source, target) {
        return Err(format!("invalid dependency edge pattern {pattern:?}"));
    }
    Ok((source, target))
}

fn edge_pattern_is_invalid(source: &str, target: &str) -> bool {
    [
        source.is_empty(),
        target.is_empty(),
        source.matches('*').count() > 1,
        target.matches('*').count() > 1,
    ]
    .into_iter()
    .any(std::convert::identity)
}

struct Tarjan<'a> {
    graph: &'a BTreeMap<String, BTreeSet<String>>,
    next_index: usize,
    indices: BTreeMap<String, usize>,
    lowlinks: BTreeMap<String, usize>,
    stack: Vec<String>,
    on_stack: BTreeSet<String>,
    components: Vec<Vec<String>>,
}

impl<'a> Tarjan<'a> {
    fn new(graph: &'a BTreeMap<String, BTreeSet<String>>) -> Self {
        Self {
            graph,
            next_index: 0,
            indices: BTreeMap::new(),
            lowlinks: BTreeMap::new(),
            stack: Vec::new(),
            on_stack: BTreeSet::new(),
            components: Vec::new(),
        }
    }

    fn visit(&mut self, node: &str) {
        self.start_node(node);
        self.visit_edges(node);
        self.finish_component(node);
    }

    fn start_node(&mut self, node: &str) {
        let index = self.next_index;
        self.next_index = self.next_index.saturating_add(1);
        self.indices.insert(node.to_string(), index);
        self.lowlinks.insert(node.to_string(), index);
        self.stack.push(node.to_string());
        self.on_stack.insert(node.to_string());
    }

    fn visit_edges(&mut self, node: &str) {
        let edges = self.graph.get(node).cloned().unwrap_or_default();
        for edge in edges {
            self.visit_edge(node, &edge);
        }
    }

    fn visit_edge(&mut self, node: &str, edge: &str) {
        if !self.indices.contains_key(edge) {
            self.visit(edge);
            self.lower_lowlink_from_child(node, edge);
        } else if self.on_stack.contains(edge) {
            self.lower_lowlink_from_index(node, edge);
        }
    }

    fn lower_lowlink_from_child(&mut self, node: &str, child: &str) {
        self.lower_lowlink(node, self.lowlinks[child]);
    }

    fn lower_lowlink_from_index(&mut self, node: &str, child: &str) {
        self.lower_lowlink(node, self.indices[child]);
    }

    fn lower_lowlink(&mut self, node: &str, child: usize) {
        if let Some(lowlink) = self.lowlinks.get_mut(node) {
            *lowlink = (*lowlink).min(child);
        }
    }

    fn finish_component(&mut self, node: &str) {
        if self.lowlinks.get(node) == self.indices.get(node) {
            let component = self.pop_component(node);
            self.components.push(component);
        }
    }

    fn pop_component(&mut self, node: &str) -> Vec<String> {
        let mut component = Vec::new();
        while let Some(member) = self.stack.pop() {
            self.on_stack.remove(&member);
            let complete = member == node;
            component.push(member);
            if complete {
                break;
            }
        }
        component
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coupling_cycles_and_instability_use_documented_formulas() {
        let packages = || {
            ["a", "b", "c", "isolated"]
                .into_iter()
                .map(|name| PackageRecord {
                    name: name.to_string(),
                    root: format!("crates/{name}"),
                })
                .collect::<Vec<_>>()
        };
        let edge = |source: &str, target: &str, internal: bool| DependencyRecord {
            package: source.to_string(),
            dependency: target.to_string(),
            source_identifier: target.to_string(),
            scope: DependencyScope::Production,
            internal,
            optional: false,
            target_gated: false,
        };
        let dependencies = vec![
            edge("a", "b", true),
            edge("b", "a", true),
            edge("c", "a", true),
            edge("c", "serde", false),
        ];
        assert_eq!(
            afferent_coupling(&packages(), &dependencies),
            BTreeMap::from([
                ("a".to_string(), 2),
                ("b".to_string(), 1),
                ("c".to_string(), 0),
                ("isolated".to_string(), 0),
            ])
        );
        assert_eq!(efferent_coupling(&packages(), &dependencies)["c"], 2);
        assert!((instability(2, 1) - (1.0 / 3.0)).abs() < 1.0e-12);
        assert!(instability(0, 0).abs() < f64::EPSILON);
        assert_eq!(
            dependency_cycles(&packages(), &dependencies),
            vec![vec!["a".to_string(), "b".to_string()]]
        );

        let self_loop = vec![edge("isolated", "isolated", true)];
        assert_eq!(
            dependency_cycles(&packages(), &self_loop),
            vec![vec!["isolated".to_string()]]
        );

        assert!(matches_pattern("adapter-*", "adapter-rust"));
        assert!(!matches_pattern("adapter-*", "analysis-dry"));
        assert!(!matches_pattern("domain*domain", "domain"));
        assert_eq!(
            parse_edge_pattern("analysis-*->adapter-*"),
            Ok(("analysis-*", "adapter-*"))
        );
        assert!(parse_edge_pattern("a**->b").is_err());
        assert!(parse_edge_pattern("not-an-edge").is_err());
    }
}
