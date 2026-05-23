use std::collections::HashMap;
use std::sync::OnceLock;

use anyhow::Context;
use petgraph::algo::astar;
use petgraph::prelude::{DiGraph, NodeIndex};
use tracing::debug;

use super::dependency::build_dependency_graph;
use super::output_helper::print_reference_location;
use super::pack_checker::PackChecker;
use super::CheckerInterface;
use crate::packs::checker::Reference;
use crate::packs::{Configuration, Sigil, Violation};

/// Cycle violations are surfaced when an *implicit* dependency (one that
/// would already produce a `dependency` violation) would close a dependency
/// cycle if adopted explicitly.
pub struct Checker {
    /// Cached graph, built lazily on first `check` invocation. The inner
    /// `Option` is `None` when the graph itself can't be built (e.g. a
    /// pack's `dependencies:` lists an unknown pack); we treat this as
    /// "no cycles to report" so a single misconfiguration doesn't blow up
    /// `pks check`. `pks validate` is the right place to surface those errors.
    graph: OnceLock<Option<OwnedDependencyGraph>>,
}

impl Checker {
    pub fn new() -> Self {
        Self {
            graph: OnceLock::new(),
        }
    }
}

impl Default for Checker {
    fn default() -> Self {
        Self::new()
    }
}

/// Owned-string view over the declared dependency graph, suitable for caching
/// behind `OnceLock`. The underlying `DependencyGraph<'a>` borrows `&Pack`s
/// and can't outlive a single `check_all` call, so we copy the names out.
struct OwnedDependencyGraph {
    graph: DiGraph<(), ()>,
    pack_to_node: HashMap<String, NodeIndex>,
    node_to_pack: HashMap<NodeIndex, String>,
}

impl OwnedDependencyGraph {
    fn build(configuration: &Configuration) -> Option<Self> {
        let (dep_graph, _self_deps) =
            match build_dependency_graph(configuration) {
                Ok(result) => result,
                Err(msg) => {
                    debug!("cycle checker disabled for this run: {}", msg);
                    return None;
                }
            };

        let pack_to_node = dep_graph
            .pack_to_node
            .iter()
            .map(|(pack, &node)| (pack.name.clone(), node))
            .collect();
        let node_to_pack = dep_graph
            .node_to_pack
            .iter()
            .map(|(&node, pack)| (node, pack.name.clone()))
            .collect();

        Some(Self {
            graph: dep_graph.graph,
            pack_to_node,
            node_to_pack,
        })
    }

    /// Shortest path from `from` to `to` along declared dependency edges,
    /// using A* with unit edge weights (effectively BFS by hop count).
    /// Returned path includes both endpoints; `None` if `to` is unreachable
    /// or either pack is unknown.
    fn shortest_path(&self, from: &str, to: &str) -> Option<Vec<String>> {
        let from_node = *self.pack_to_node.get(from)?;
        let to_node = *self.pack_to_node.get(to)?;

        let (_, node_path) =
            astar(&self.graph, from_node, |n| n == to_node, |_| 1, |_| 0)?;

        Some(
            node_path
                .into_iter()
                .map(|n| self.node_to_pack[&n].clone())
                .collect(),
        )
    }
}

impl CheckerInterface for Checker {
    fn check(
        &self,
        reference: &Reference,
        configuration: &Configuration,
        _sigils: &HashMap<std::path::PathBuf, Vec<Sigil>>,
    ) -> anyhow::Result<Option<Violation>> {
        let pack_checker =
            PackChecker::new(configuration, reference, &self.violation_type())?;
        if !pack_checker.checkable()? {
            return Ok(None);
        }
        let defining_pack = pack_checker.defining_pack.unwrap();
        let referencing_pack = pack_checker.referencing_pack;

        // A cycle violation only piggybacks on an actual dependency violation:
        // if `referencing_pack` already declares `defining_pack` as a
        // dependency (or marks it ignored), there is no implicit edge to
        // worry about.
        let already_declared =
            referencing_pack.dependencies.contains(&defining_pack.name)
                || referencing_pack
                    .ignored_dependencies
                    .contains(&defining_pack.name);
        if already_declared {
            return Ok(None);
        }

        let relative_defining_file =
            reference.relative_defining_file.as_ref().context(format!(
                "expected a relative defining file for defining pack: {}",
                defining_pack.name
            ))?;
        if referencing_pack
            .is_ignored(relative_defining_file, &self.violation_type())?
        {
            return Ok(None);
        }

        // Lazily initialize the graph cache the first time we need it. Built
        // once, amortizes across the whole `check_all` run.
        let graph = self
            .graph
            .get_or_init(|| OwnedDependencyGraph::build(configuration));
        let graph = match graph {
            Some(g) => g,
            None => return Ok(None),
        };

        // Cycle exists iff `defining_pack` already (transitively) depends on
        // `referencing_pack` via declared edges. Adopting the implicit edge
        // referencing -> defining would close the loop. `astar` answers both
        // "is it reachable?" and "what's the shortest path?" in one call.
        let declared_path = match graph
            .shortest_path(&defining_pack.name, &referencing_pack.name)
        {
            Some(p) => p,
            None => return Ok(None),
        };

        // Build the cycle path: implicit edge first, then the declared path
        // back from defining to referencing.
        let mut cycle_path: Vec<String> = vec![referencing_pack.name.clone()];
        cycle_path.extend(declared_path);
        let cycle_detail = cycle_path.join(" -> ");

        let loc = print_reference_location(reference);
        let message = format!(
            "{}Cycle violation: adding `{}` as a dependency of `{}` would close a cycle: {}",
            loc, defining_pack.name, referencing_pack.name, cycle_detail,
        );

        Ok(Some(Violation {
            message,
            identifier: pack_checker
                .violation_identifier_with_details(Some(cycle_detail)),
            source_location: reference.source_location.clone(),
        }))
    }

    fn violation_type(&self) -> String {
        "cycle".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::checker::common_test::tests::{
        build_expected_violation_with_details, default_defining_pack,
        default_referencing_pack, test_check, TestChecker,
    };
    use crate::packs::pack::CheckerSetting;
    use crate::packs::{configuration, Pack, PackSet};
    use std::collections::HashSet;
    use std::path::PathBuf;

    /// Three packs in a closed cycle: foo declares bar, bar declares baz, baz
    /// declares foo. A reference from foo into bar would normally also be a
    /// vanilla dependency violation; here we are isolating the cycle case by
    /// skipping a separate dependency violation and only asserting the cycle
    /// emission.
    fn cycle_configuration(
        referencing_pack_name: &str,
        defining_pack_name: &str,
    ) -> Configuration {
        let foo = Pack {
            name: "packs/foo".to_owned(),
            relative_path: PathBuf::from("packs/foo"),
            enforce_dependencies: Some(CheckerSetting::True),
            dependencies: HashSet::from(["packs/bar".to_string()]),
            ..Pack::default()
        };
        let bar = Pack {
            name: "packs/bar".to_owned(),
            relative_path: PathBuf::from("packs/bar"),
            enforce_dependencies: Some(CheckerSetting::True),
            dependencies: HashSet::from(["packs/baz".to_string()]),
            ..Pack::default()
        };
        let baz = Pack {
            name: "packs/baz".to_owned(),
            relative_path: PathBuf::from("packs/baz"),
            enforce_dependencies: Some(CheckerSetting::True),
            dependencies: HashSet::from(["packs/foo".to_string()]),
            ..Pack::default()
        };
        let root = Pack {
            name: ".".to_owned(),
            ..Pack::default()
        };

        // Resolve referencing/defining packs from the just-built fixtures so
        // the TestChecker harness can plug them in.
        let _ = (referencing_pack_name, defining_pack_name);

        Configuration {
            pack_set: PackSet::build(
                HashSet::from_iter(vec![root, foo, bar, baz]),
                HashMap::new(),
            )
            .unwrap(),
            ..Configuration::default()
        }
    }

    #[test]
    fn direct_mutual_dep_emits_cycle() -> anyhow::Result<()> {
        // packs/bar declares packs/foo. packs/foo references something from
        // packs/bar without declaring it: adopting that edge closes the
        // cycle foo -> bar -> foo.
        let foo = Pack {
            name: "packs/foo".to_owned(),
            relative_path: PathBuf::from("packs/foo"),
            enforce_dependencies: Some(CheckerSetting::True),
            ..default_referencing_pack()
        };
        let bar = Pack {
            name: "packs/bar".to_owned(),
            relative_path: PathBuf::from("packs/bar"),
            enforce_dependencies: Some(CheckerSetting::True),
            dependencies: HashSet::from(["packs/foo".to_string()]),
            ..default_defining_pack()
        };
        let root = Pack {
            name: ".".to_owned(),
            ..Pack::default()
        };
        let configuration = Configuration {
            pack_set: PackSet::build(
                HashSet::from_iter(vec![root, foo.clone(), bar.clone()]),
                HashMap::new(),
            )
            .unwrap(),
            ..Configuration::default()
        };

        let mut test_checker = TestChecker {
            reference: None,
            configuration: Some(configuration),
            referenced_constant_name: Some(String::from("::Bar")),
            defining_pack: Some(bar),
            referencing_pack: foo,
            expected_violation: Some(build_expected_violation_with_details(
                "packs/foo/app/services/foo.rb:3:1\nCycle violation: adding `packs/bar` as a dependency of `packs/foo` would close a cycle: packs/foo -> packs/bar -> packs/foo".to_string(),
                "cycle".to_string(),
                false,
                Some("packs/foo -> packs/bar -> packs/foo".to_string()),
            )),
        };
        test_check(&Checker::new(), &mut test_checker)
    }

    #[test]
    fn transitive_cycle_emits_full_path() -> anyhow::Result<()> {
        // foo -> bar -> baz declared. foo references baz: adopting it closes
        // foo -> baz -> ... -> foo? No — declared chain is foo -> bar -> baz,
        // so foo->baz alone does NOT close a cycle. Instead, build a config
        // where baz declares foo, and foo references bar: adopting it closes
        // foo -> bar -> baz -> foo (length 3 cycle including the implicit
        // edge).
        let configuration = cycle_configuration("packs/foo", "packs/bar");
        let foo = configuration
            .pack_set
            .for_pack("packs/foo")
            .unwrap()
            .clone();
        let bar = configuration
            .pack_set
            .for_pack("packs/bar")
            .unwrap()
            .clone();

        // Strip foo's declared dep on bar so we see this as an implicit ref.
        let foo_implicit = Pack {
            dependencies: HashSet::new(),
            ..foo
        };
        let mut packs: HashSet<Pack> =
            configuration.pack_set.packs.iter().cloned().collect();
        packs.retain(|p| p.name != "packs/foo");
        packs.insert(foo_implicit.clone());

        let configuration = Configuration {
            pack_set: PackSet::build(packs, HashMap::new()).unwrap(),
            ..Configuration::default()
        };

        let mut test_checker = TestChecker {
            reference: None,
            configuration: Some(configuration),
            referenced_constant_name: Some(String::from("::Bar")),
            defining_pack: Some(bar),
            referencing_pack: foo_implicit,
            expected_violation: Some(build_expected_violation_with_details(
                "packs/foo/app/services/foo.rb:3:1\nCycle violation: adding `packs/bar` as a dependency of `packs/foo` would close a cycle: packs/foo -> packs/bar -> packs/baz -> packs/foo".to_string(),
                "cycle".to_string(),
                false,
                Some("packs/foo -> packs/bar -> packs/baz -> packs/foo".to_string()),
            )),
        };
        test_check(&Checker::new(), &mut test_checker)
    }

    #[test]
    fn no_cycle_emits_nothing() -> anyhow::Result<()> {
        // foo references bar implicitly; nothing depends back on foo, so
        // adopting the edge wouldn't close a cycle. Should NOT emit a cycle
        // violation (the dependency checker will still emit a `dependency`
        // violation, but that's not this checker's concern).
        let mut test_checker = TestChecker {
            reference: None,
            configuration: None,
            referenced_constant_name: Some(String::from("::Bar")),
            defining_pack: Some(Pack {
                name: "packs/bar".to_owned(),
                ..default_defining_pack()
            }),
            referencing_pack: Pack {
                name: "packs/foo".to_owned(),
                relative_path: PathBuf::from("packs/foo"),
                enforce_dependencies: Some(CheckerSetting::True),
                ..default_referencing_pack()
            },
            ..Default::default()
        };
        test_check(&Checker::new(), &mut test_checker)
    }

    #[test]
    fn already_declared_dependency_emits_nothing() -> anyhow::Result<()> {
        // foo declares bar AND bar declares foo (an existing cycle in the
        // declared graph). foo references bar — but bar is already declared,
        // so this isn't an implicit dep. We should not emit a cycle violation
        // here; cycle violations only piggyback on implicit deps.
        let foo = Pack {
            name: "packs/foo".to_owned(),
            relative_path: PathBuf::from("packs/foo"),
            enforce_dependencies: Some(CheckerSetting::True),
            dependencies: HashSet::from(["packs/bar".to_string()]),
            ..default_referencing_pack()
        };
        let bar = Pack {
            name: "packs/bar".to_owned(),
            relative_path: PathBuf::from("packs/bar"),
            enforce_dependencies: Some(CheckerSetting::True),
            dependencies: HashSet::from(["packs/foo".to_string()]),
            ..default_defining_pack()
        };
        let root = Pack {
            name: ".".to_owned(),
            ..Pack::default()
        };
        let configuration = Configuration {
            pack_set: PackSet::build(
                HashSet::from_iter(vec![root, foo.clone(), bar.clone()]),
                HashMap::new(),
            )
            .unwrap(),
            ..Configuration::default()
        };
        let mut test_checker = TestChecker {
            reference: None,
            configuration: Some(configuration),
            referenced_constant_name: Some(String::from("::Bar")),
            defining_pack: Some(bar),
            referencing_pack: foo,
            ..Default::default()
        };
        test_check(&Checker::new(), &mut test_checker)
    }

    /// Sanity check that the configuration helper builds without panicking.
    /// Detached from a real `pks` execution path.
    #[test]
    fn cycle_configuration_smoke() {
        let _ = cycle_configuration("packs/foo", "packs/bar");
        // Use the existing on-disk cycle fixture as another reachability sanity
        // check: this path uses `configuration::get` end-to-end.
        let _ = configuration::get(
            PathBuf::from("tests/fixtures/app_with_dependency_cycles")
                .canonicalize()
                .expect("Could not canonicalize path")
                .as_path(),
            &1,
        )
        .unwrap();
    }
}
