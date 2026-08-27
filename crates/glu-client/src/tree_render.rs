use crate::{state::installed::DependencyTreeNode, style};

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
    /// Include edge requirements (`>= 1.2.3`) when present.
    pub verbose: bool,
    /// Root branch convention for decorated output.
    pub root_style: RootStyle,
}

impl TreeRenderOptions {
    pub fn decorated(root_style: RootStyle) -> Self {
        Self {
            decorated: true,
            direct: false,
            verbose: false,
            root_style,
        }
    }

    pub fn plain() -> Self {
        Self {
            decorated: false,
            direct: false,
            verbose: false,
            root_style: RootStyle::SiblingBranches,
        }
    }
}

/// Render dependency tree lines with one shared branch/version/marker policy.
pub fn render_dependency_tree(
    nodes: &[DependencyTreeNode],
    options: TreeRenderOptions,
) -> Vec<String> {
    let mut lines = Vec::new();
    render_nodes(nodes, "", 0, options, &mut lines);
    lines
}

fn render_nodes(
    nodes: &[DependencyTreeNode],
    prefix: &str,
    depth: usize,
    options: TreeRenderOptions,
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
        lines.push(render_node_line(node, prefix, last, plain_root, options));
        if !options.direct || depth == 0 {
            render_nodes(&node.children, &child_prefix, depth + 1, options, lines);
        }
    }
}

fn render_node_line(
    node: &DependencyTreeNode,
    prefix: &str,
    last: bool,
    plain_root: bool,
    options: TreeRenderOptions,
) -> String {
    let version = if node.version.is_empty() {
        String::new()
    } else if options.decorated {
        format!(" {}", style::dim(&node.version))
    } else {
        format!(" {}", node.version)
    };
    let requires = if options.verbose {
        node.requires
            .as_ref()
            .map(|requirement| {
                if options.decorated {
                    format!(" {}", style::dim(requirement))
                } else {
                    format!(" ({requirement})")
                }
            })
            .unwrap_or_default()
    } else {
        String::new()
    };
    if !options.decorated || plain_root {
        return format!("{}{}{}", node.name, version, requires);
    }

    let branch = if last { "└── " } else { "├── " };
    let marker = if node.already_shown {
        format!(" {}", style::cyan("↰"))
    } else {
        String::new()
    };
    format!(
        "{}{}{}{}{}",
        style::dim(&format!("{prefix}{branch}")),
        node.name,
        version,
        requires,
        marker
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(name: &str, children: Vec<DependencyTreeNode>) -> DependencyTreeNode {
        DependencyTreeNode {
            name: name.to_string(),
            version: format!("{name}-1"),
            children,
            already_shown: false,
            requires: None,
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
    fn plain_verbose_direct_output_keeps_only_one_level() {
        let mut child = node("child", vec![node("grandchild", vec![])]);
        child.requires = Some(">= 1.0".to_string());
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
