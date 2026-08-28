use crate::{dependency_query::DependencyTreeNode, style};
use glu_core::MinimumVersion;
use std::collections::BTreeSet;

/// Format a typed dependency floor at the presentation boundary.
pub fn format_minimum_version(minimum: &MinimumVersion) -> String {
    let mut floor = format!(">= {}", minimum.version);
    if let Some(revision) = minimum.revision.filter(|revision| *revision > 0) {
        floor.push_str(&format!("_{revision}"));
    }
    floor
}

/// How root nodes should be drawn when rendering a decorated tree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RootStyle {
    /// Roots are plain package lines with no leading branch. Children are still
    /// rendered as a tree. This is the `glu ls --tree` rootless forest shape.
    Plain,
    /// Roots are siblings: every root except the last uses `├──` and keeps the
    /// vertical guide alive for its children.
    SiblingBranches,
    /// Every root is drawn as a standalone final branch (`└──`). Install/update
    /// plans historically render each requested root this way, even when a plan
    /// has multiple roots; keep that shape for exact CLI compatibility.
    AlwaysLast,
}

/// Rendering policy for [`DependencyTreeNode`] trees.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TreeRenderOptions {
    /// When true, draw branch guides and dim/cyan terminal decorations. When
    /// false, render clean `name version` lines for piped output.
    pub decorated: bool,
    /// Show only one dependency level below each supplied root.
    pub direct: bool,
    /// Include the requiring package's minimum (`>= 1.2.3`) when present.
    pub verbose: bool,
    /// Include each node's concrete version.
    pub show_versions: bool,
    /// Label verbose metadata as dependency requirements and installed
    /// versions instead of presenting a node version as a requirement.
    pub version_label: Option<&'static str>,
    /// Root branch convention for decorated output.
    pub root_style: RootStyle,
}

impl TreeRenderOptions {
    pub fn decorated(root_style: RootStyle) -> Self {
        Self {
            decorated: true,
            direct: false,
            verbose: false,
            show_versions: true,
            version_label: None,
            root_style,
        }
    }

    pub fn plain() -> Self {
        Self {
            decorated: false,
            direct: false,
            verbose: false,
            show_versions: true,
            version_label: None,
            root_style: RootStyle::SiblingBranches,
        }
    }
}

/// Render dependency tree lines with one shared branch/version/marker policy.
pub fn render_dependency_tree(
    nodes: &[DependencyTreeNode],
    options: TreeRenderOptions,
) -> Vec<String> {
    render_dependency_tree_with_context(nodes, options, &BTreeSet::new())
}

/// Render a tree while retaining unchanged nodes needed to connect visible
/// mutation work. Context rows are fully dimmed and explicitly annotated so
/// redirected output remains unambiguous without ANSI styling.
pub fn render_dependency_tree_with_context(
    nodes: &[DependencyTreeNode],
    options: TreeRenderOptions,
    context: &BTreeSet<(glu_core::PackageKey, String)>,
) -> Vec<String> {
    let mut lines = Vec::new();
    render_nodes(nodes, "", 0, options, context, &mut lines);
    lines
}

fn render_nodes(
    nodes: &[DependencyTreeNode],
    prefix: &str,
    depth: usize,
    options: TreeRenderOptions,
    context: &BTreeSet<(glu_core::PackageKey, String)>,
    lines: &mut Vec<String>,
) {
    for (index, node) in nodes.iter().enumerate() {
        let sibling_last = index + 1 == nodes.len();
        let last = if depth == 0 && options.root_style == RootStyle::AlwaysLast {
            true
        } else {
            sibling_last
        };
        let plain_root = depth == 0 && options.root_style == RootStyle::Plain;
        let child_prefix = if plain_root {
            String::new()
        } else {
            format!("{prefix}{}", if last { "    " } else { "│   " })
        };
        lines.push(render_node_line(
            node, plain_root, prefix, last, options, context,
        ));
        if !options.direct || depth == 0 {
            render_nodes(
                &node.children,
                &child_prefix,
                depth + 1,
                options,
                context,
                lines,
            );
        }
    }
}

fn render_node_line(
    node: &DependencyTreeNode,
    plain_root: bool,
    prefix: &str,
    last: bool,
    options: TreeRenderOptions,
    context: &BTreeSet<(glu_core::PackageKey, String)>,
) -> String {
    let version = (options.show_versions && !node.version.is_empty()).then_some(&node.version);
    let requirement = options
        .verbose
        .then(|| {
            node.incoming
                .as_ref()
                .and_then(|edge| edge.minimum.as_ref())
                .map(format_minimum_version)
        })
        .flatten();
    let metadata = if let Some(label) = options.version_label {
        let mut parts = Vec::new();
        if let Some(requirement) = requirement.as_ref() {
            parts.push(format!("requires {requirement}"));
        }
        if let Some(version) = version {
            parts.push(format!("{version} {label}"));
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!(" {}", style::dim(&format!("({})", parts.join("; "))))
        }
    } else {
        let version = version
            .map(|version| {
                if options.decorated {
                    format!(" {}", style::dim(version))
                } else {
                    format!(" {version}")
                }
            })
            .unwrap_or_default();
        let requirement = requirement
            .as_ref()
            .map(|requirement| {
                if options.decorated {
                    format!(" {}", style::dim(requirement))
                } else {
                    format!(" ({requirement})")
                }
            })
            .unwrap_or_default();
        format!("{version}{requirement}")
    };
    let is_context = context.contains(&(node.package_key.clone(), node.version.clone()));
    if !options.decorated || plain_root {
        let line = format!(
            "{}{}{}",
            node.name,
            metadata,
            if is_context { " (installed)" } else { "" }
        );
        return if is_context { style::dim(&line) } else { line };
    }

    let branch = if last { "└── " } else { "├── " };
    let marker = if node.already_shown {
        format!(" {}", style::cyan("↰"))
    } else {
        String::new()
    };
    if is_context {
        style::dim(&format!(
            "{prefix}{branch}{}{} (installed){marker}",
            node.name, metadata
        ))
    } else {
        format!(
            "{}{}{}{}",
            style::dim(&format!("{prefix}{branch}")),
            node.name,
            metadata,
            marker
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, children: Vec<DependencyTreeNode>) -> DependencyTreeNode {
        DependencyTreeNode {
            package_key: glu_core::PackageKey(format!("package:{name}")),
            package: glu_core::PackageId(format!("pkg:test/{name}@{name}-1")),
            name: name.to_string(),
            canonical_name: glu_core::PackageName(name.to_string()),
            version: format!("{name}-1"),
            children,
            already_shown: false,
            incoming: None,
        }
    }

    #[test]
    fn sibling_roots_keep_vertical_guides() {
        let tree = vec![node("a", vec![node("a1", vec![])]), node("b", vec![])];
        let lines = render_dependency_tree(
            &tree,
            TreeRenderOptions::decorated(RootStyle::SiblingBranches),
        );
        assert_eq!(
            lines,
            vec![
                "├── a a-1".to_string(),
                "│   └── a1 a1-1".to_string(),
                "└── b b-1".to_string(),
            ]
        );
    }

    #[test]
    fn plain_roots_render_rootless_forest() {
        let tree = vec![node("a", vec![node("a1", vec![])]), node("b", vec![])];
        let lines = render_dependency_tree(&tree, TreeRenderOptions::decorated(RootStyle::Plain));
        assert_eq!(
            lines,
            vec![
                "a a-1".to_string(),
                "└── a1 a1-1".to_string(),
                "b b-1".to_string(),
            ]
        );
    }

    #[test]
    fn plan_roots_are_each_rendered_as_standalone_last_branches() {
        let tree = vec![node("a", vec![node("a1", vec![])]), node("b", vec![])];
        let lines =
            render_dependency_tree(&tree, TreeRenderOptions::decorated(RootStyle::AlwaysLast));
        assert_eq!(
            lines,
            vec![
                "└── a a-1".to_string(),
                "    └── a1 a1-1".to_string(),
                "└── b b-1".to_string(),
            ]
        );
    }

    #[test]
    fn context_nodes_are_annotated_without_color() {
        let tree = vec![node(
            "root",
            vec![node("context", vec![node("work", vec![])])],
        )];
        let context = BTreeSet::from([(
            glu_core::PackageKey("package:context".to_string()),
            "context-1".to_string(),
        )]);
        let lines = render_dependency_tree_with_context(
            &tree,
            TreeRenderOptions::decorated(RootStyle::Plain),
            &context,
        );
        assert_eq!(
            lines,
            vec![
                "root root-1".to_string(),
                "└── context context-1 (installed)".to_string(),
                "    └── work work-1".to_string(),
            ]
        );
    }

    #[test]
    fn plain_verbose_direct_output_keeps_only_one_level() {
        let mut child = node("child", vec![node("grandchild", vec![])]);
        child.incoming = Some(crate::dependency_query::DependencyTreeEdge {
            requested_as: glu_core::PackageSelector("child".to_string()),
            reversed: false,
            minimum: Some(MinimumVersion {
                version: "1.0".to_string(),
                revision: None,
            }),
        });
        let tree = vec![node("root", vec![child])];
        let mut options = TreeRenderOptions::plain();
        options.direct = true;
        options.verbose = true;
        let lines = render_dependency_tree(&tree, options);
        assert_eq!(
            lines,
            vec![
                "root root-1".to_string(),
                "child child-1 (>= 1.0)".to_string(),
            ]
        );
    }
}
