//! Connected-components partitioning of test methods by `@depends`.
//!
//! When the runner is about to dispatch a test class, it asks the worker
//! for `{name, depends: [...]}` per method, then groups methods into
//! dependency components: every chain of methods linked by depends ends
//! up in one component. Components are the unit of dispatch — one
//! component goes to one worker so PHPUnit's `@depends` value-passing
//! works (depended-on methods always run before dependents, in the same
//! process). Methods without depends become singleton components and
//! parallelize freely.

use std::collections::HashMap;

/// Partition `methods` into disjoint groups based on the dependency
/// relation. Two methods end up in the same group iff there's a chain
/// of `depends` links connecting them (either direction).
///
/// `depends` maps method name → list of method names it depends on.
/// Any name in `depends` values that isn't in `methods` is ignored
/// (e.g., depends on an inherited method we didn't enumerate).
///
/// Groups are returned in stable order: the first method to appear in
/// `methods` determines the position of its group. Method order within
/// a group is preserved from `methods`.
pub fn partition_by_depends(
    methods: &[String],
    depends: &HashMap<String, Vec<String>>,
) -> Vec<Vec<String>> {
    // Index each method to its position in `methods` for stable ordering.
    let positions: HashMap<&str, usize> = methods
        .iter()
        .enumerate()
        .map(|(i, m)| (m.as_str(), i))
        .collect();

    // Union-find over the method indices.
    let n = methods.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut Vec<usize>, mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]]; // path compression
            i = parent[i];
        }
        i
    }

    fn union(parent: &mut Vec<usize>, a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[ra] = rb;
        }
    }

    for (i, method) in methods.iter().enumerate() {
        if let Some(deps) = depends.get(method) {
            for dep in deps {
                if let Some(&j) = positions.get(dep.as_str()) {
                    union(&mut parent, i, j);
                }
            }
        }
    }

    // Collect into groups keyed by root, preserving discovery order.
    let mut groups: Vec<Vec<String>> = Vec::new();
    let mut group_index: HashMap<usize, usize> = HashMap::new();
    for (i, method) in methods.iter().enumerate() {
        let root = find(&mut parent, i);
        let idx = match group_index.get(&root) {
            Some(&idx) => idx,
            None => {
                let new_idx = groups.len();
                group_index.insert(root, new_idx);
                groups.push(Vec::new());
                new_idx
            }
        };
        groups[idx].push(method.clone());
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_depends_yields_singleton_groups() {
        let methods = vec!["testA".to_string(), "testB".to_string(), "testC".to_string()];
        let depends = HashMap::new();
        let groups = partition_by_depends(&methods, &depends);
        assert_eq!(groups, vec![
            vec!["testA".to_string()],
            vec!["testB".to_string()],
            vec!["testC".to_string()],
        ]);
    }

    #[test]
    fn linear_chain_stays_in_one_group() {
        let methods = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let mut depends = HashMap::new();
        depends.insert("b".to_string(), vec!["a".to_string()]);
        depends.insert("c".to_string(), vec!["b".to_string()]);
        let groups = partition_by_depends(&methods, &depends);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0], vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    }

    #[test]
    fn independent_chains_split_into_separate_groups() {
        let methods = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let mut depends = HashMap::new();
        depends.insert("b".into(), vec!["a".into()]);
        depends.insert("d".into(), vec!["c".into()]);
        let groups = partition_by_depends(&methods, &depends);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0], vec!["a".to_string(), "b".to_string()]);
        assert_eq!(groups[1], vec!["c".to_string(), "d".to_string()]);
    }

    #[test]
    fn depends_on_unknown_method_is_ignored() {
        let methods = vec!["a".into(), "b".into()];
        let mut depends = HashMap::new();
        depends.insert("a".into(), vec!["inheritedHelper".into()]);
        let groups = partition_by_depends(&methods, &depends);
        // No union happened (inheritedHelper not in methods); singletons.
        assert_eq!(groups.len(), 2);
    }

    #[test]
    fn diamond_chain_unifies_through_intermediate() {
        // a → b → d, a → c → d. All four end up in one component.
        let methods = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let mut depends = HashMap::new();
        depends.insert("b".into(), vec!["a".into()]);
        depends.insert("c".into(), vec!["a".into()]);
        depends.insert("d".into(), vec!["b".into(), "c".into()]);
        let groups = partition_by_depends(&methods, &depends);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].len(), 4);
    }
}
