#![deny(unsafe_code)]
//! Dev automation (docs/02 §3): `arch-test` (dependency-direction rules, NFR-08),
//! `codegen` (contracts → TypeScript, lands in F0.3), `golden` (trace runner, F0.9).

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

fn main() -> anyhow::Result<()> {
    let task = std::env::args().nth(1).unwrap_or_default();
    match task.as_str() {
        "arch-test" => arch_test(),
        "codegen" => anyhow::bail!("xtask codegen lands with F0.3 (contracts seed)"),
        "golden" => anyhow::bail!("xtask golden lands with F0.9 (CI pipeline)"),
        _ => anyhow::bail!("usage: cargo xtask <arch-test|codegen|golden>"),
    }
}

/// Direct dependencies of one workspace crate, split into workspace-internal
/// edges and external crates. Normal deps only — dev/build deps may be looser.
#[derive(Debug, Default, PartialEq)]
struct CrateDeps {
    workspace: BTreeSet<String>,
    external: BTreeSet<String>,
}

type DepGraph = BTreeMap<String, CrateDeps>;

/// The dependency-direction rules from docs/02 §3 (NFR-08):
/// domain ← application ← {contracts, infra, adapters} ← jarvisd.
/// `workspace_allowed` = full allowed set of workspace-internal deps.
/// `external_allowed` = Some(allowlist) for purity-constrained crates
/// (CLAUDE.md invariant 3), None = externals unconstrained here (cargo deny
/// still gates licenses/advisories for everything).
struct Rule {
    krate: &'static str,
    workspace_allowed: &'static [&'static str],
    external_allowed: Option<&'static [&'static str]>,
}

const RULES: &[Rule] = &[
    Rule {
        krate: "jarvis-domain",
        workspace_allowed: &[],
        external_allowed: Some(&["serde", "thiserror"]),
    },
    Rule {
        krate: "jarvis-application",
        workspace_allowed: &["jarvis-domain"],
        // async traits and cancellation primitives only — never sqlx, axum,
        // reqwest, rmcp, tokio proper, or provider SDKs.
        external_allowed: Some(&[
            "serde",
            "thiserror",
            "async-trait",
            "tokio-util",
            "futures-core",
        ]),
    },
    Rule {
        krate: "jarvis-contracts",
        workspace_allowed: &["jarvis-domain"],
        external_allowed: None,
    },
    Rule {
        krate: "jarvis-infra",
        workspace_allowed: &["jarvis-domain", "jarvis-application"],
        external_allowed: None,
    },
    Rule {
        krate: "jarvis-adapters",
        workspace_allowed: &["jarvis-domain", "jarvis-application"],
        external_allowed: None,
    },
    Rule {
        krate: "jarvisd",
        workspace_allowed: &[
            "jarvis-domain",
            "jarvis-application",
            "jarvis-contracts",
            "jarvis-infra",
            "jarvis-adapters",
        ],
        external_allowed: None,
    },
    Rule {
        krate: "jarvis-agent",
        workspace_allowed: &["jarvis-contracts"],
        external_allowed: None,
    },
    Rule {
        krate: "xtask",
        workspace_allowed: &[],
        external_allowed: None,
    },
];

fn arch_test() -> anyhow::Result<()> {
    let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()?;
    anyhow::ensure!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let graph = parse_graph(&serde_json::from_slice(&output.stdout)?)?;
    let violations = check(&graph);
    if violations.is_empty() {
        println!("arch-test: {} crates, dependency rules hold", graph.len());
        Ok(())
    } else {
        for v in &violations {
            eprintln!("arch-test violation: {v}");
        }
        anyhow::bail!("{} dependency-rule violation(s)", violations.len());
    }
}

fn parse_graph(metadata: &serde_json::Value) -> anyhow::Result<DepGraph> {
    let packages = metadata["packages"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("cargo metadata: missing packages array"))?;
    let workspace_names: BTreeSet<String> = packages
        .iter()
        .filter_map(|p| p["name"].as_str().map(String::from))
        .collect();

    let mut graph = DepGraph::new();
    for package in packages {
        let name = package["name"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("package without name"))?;
        let mut deps = CrateDeps::default();
        for dep in package["dependencies"].as_array().into_iter().flatten() {
            let dep_name = dep["name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("dependency without name"))?;
            // Workspace-internal edges count for every kind — a dev/build dep on a
            // higher layer is still an inverted architecture edge. External deps are
            // checked for normal kind only (test/build tooling may be looser); note
            // transitive purity is deliberately out of scope here — direct deps are
            // the structural gate, cargo deny gates the full graph's supply chain.
            if workspace_names.contains(dep_name) {
                deps.workspace.insert(dep_name.into());
            } else if dep["kind"].is_null() {
                deps.external.insert(dep_name.into());
            }
        }
        graph.insert(name.into(), deps);
    }
    Ok(graph)
}

fn check(graph: &DepGraph) -> Vec<String> {
    let mut violations = Vec::new();
    for rule in RULES {
        let Some(deps) = graph.get(rule.krate) else {
            violations.push(format!("workspace crate `{}` is missing", rule.krate));
            continue;
        };
        for dep in &deps.workspace {
            if !rule.workspace_allowed.contains(&dep.as_str()) {
                violations.push(format!(
                    "`{}` must not depend on workspace crate `{dep}` (docs/02 §3)",
                    rule.krate
                ));
            }
        }
        if let Some(allowed) = rule.external_allowed {
            for dep in &deps.external {
                if !allowed.contains(&dep.as_str()) {
                    violations.push(format!(
                        "`{}` must not depend on `{dep}` — purity rule (CLAUDE.md invariant 3, NFR-08)",
                        rule.krate
                    ));
                }
            }
        }
    }
    // Any crate not covered by a rule is a layout change that must be deliberate.
    for name in graph.keys() {
        if !RULES.iter().any(|r| r.krate == name) {
            violations.push(format!(
                "crate `{name}` has no arch-test rule — add one in xtask (docs/02 §3)"
            ));
        }
    }
    violations
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(entries: &[(&str, &[&str], &[&str])]) -> DepGraph {
        entries
            .iter()
            .map(|(name, ws, ext)| {
                (
                    name.to_string(),
                    CrateDeps {
                        workspace: ws.iter().map(|s| s.to_string()).collect(),
                        external: ext.iter().map(|s| s.to_string()).collect(),
                    },
                )
            })
            .collect()
    }

    fn full_clean_graph() -> DepGraph {
        graph(&[
            ("jarvis-domain", &[], &["serde", "thiserror"]),
            ("jarvis-application", &["jarvis-domain"], &["thiserror"]),
            (
                "jarvis-contracts",
                &["jarvis-domain"],
                &["serde", "schemars"],
            ),
            (
                "jarvis-infra",
                &["jarvis-domain", "jarvis-application"],
                &["sqlx"],
            ),
            (
                "jarvis-adapters",
                &["jarvis-domain", "jarvis-application"],
                &["reqwest"],
            ),
            (
                "jarvisd",
                &[
                    "jarvis-domain",
                    "jarvis-application",
                    "jarvis-contracts",
                    "jarvis-infra",
                    "jarvis-adapters",
                ],
                &["axum", "anyhow"],
            ),
            ("jarvis-agent", &["jarvis-contracts"], &["anyhow"]),
            ("xtask", &[], &["anyhow", "serde_json"]),
        ])
    }

    #[test]
    fn clean_graph_passes() {
        assert_eq!(check(&full_clean_graph()), Vec::<String>::new());
    }

    #[test]
    fn domain_gains_io_dependency_fails() {
        let mut g = full_clean_graph();
        g.get_mut("jarvis-domain")
            .unwrap()
            .external
            .insert("sqlx".into());
        let violations = check(&g);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("jarvis-domain"));
        assert!(violations[0].contains("sqlx"));
    }

    #[test]
    fn application_gains_axum_fails() {
        let mut g = full_clean_graph();
        g.get_mut("jarvis-application")
            .unwrap()
            .external
            .insert("axum".into());
        assert_eq!(check(&g).len(), 1);
    }

    #[test]
    fn inverted_workspace_edge_fails() {
        let mut g = full_clean_graph();
        g.get_mut("jarvis-domain")
            .unwrap()
            .workspace
            .insert("jarvis-infra".into());
        let violations = check(&g);
        assert_eq!(violations.len(), 1);
        assert!(violations[0].contains("must not depend on workspace crate"));
    }

    #[test]
    fn agent_reaching_past_contracts_fails() {
        let mut g = full_clean_graph();
        g.get_mut("jarvis-agent")
            .unwrap()
            .workspace
            .insert("jarvis-infra".into());
        assert_eq!(check(&g).len(), 1);
    }

    #[test]
    fn parse_keeps_workspace_dev_edges_and_drops_external_dev_deps() {
        let metadata = serde_json::json!({
            "packages": [
                {
                    "name": "jarvis-domain",
                    "dependencies": [
                        { "name": "serde", "kind": null },
                        { "name": "criterion", "kind": "dev" },
                        { "name": "jarvis-infra", "kind": "dev" },
                    ],
                },
                { "name": "jarvis-infra", "dependencies": [] },
            ],
        });
        let graph = parse_graph(&metadata).unwrap();
        let domain = &graph["jarvis-domain"];
        // The inverted dev edge must be visible to check(); external dev deps are not.
        assert!(domain.workspace.contains("jarvis-infra"));
        assert!(domain.external.contains("serde"));
        assert!(!domain.external.contains("criterion"));
        assert!(
            check(&graph)
                .iter()
                .any(|v| v.contains("jarvis-domain") && v.contains("jarvis-infra"))
        );
    }

    #[test]
    fn missing_crate_and_unknown_crate_are_flagged() {
        let mut g = full_clean_graph();
        g.remove("jarvis-contracts");
        g.insert("jarvis-new-thing".into(), CrateDeps::default());
        let violations = check(&g);
        assert_eq!(violations.len(), 2);
        assert!(violations.iter().any(|v| v.contains("is missing")));
        assert!(violations.iter().any(|v| v.contains("no arch-test rule")));
    }
}
