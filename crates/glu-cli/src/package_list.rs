use glu_client::style;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PackageIdentity {
    Name(String),
    Rename { from: String, to: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PackageVersion {
    None,
    Current(String),
    Change { from: String, to: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PackageListItem {
    pub(crate) identity: PackageIdentity,
    pub(crate) version: PackageVersion,
    pub(crate) annotation: Option<String>,
    pub(crate) emphasized: bool,
}

impl PackageListItem {
    pub(crate) fn name(name: impl Into<String>) -> Self {
        Self {
            identity: PackageIdentity::Name(name.into()),
            version: PackageVersion::None,
            annotation: None,
            emphasized: false,
        }
    }

    pub(crate) fn package(name: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            identity: PackageIdentity::Name(name.into()),
            version: PackageVersion::Current(version.into()),
            annotation: None,
            emphasized: false,
        }
    }

    pub(crate) fn update(
        name: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        Self {
            identity: PackageIdentity::Name(name.into()),
            version: PackageVersion::Change {
                from: from.into(),
                to: to.into(),
            },
            annotation: None,
            emphasized: false,
        }
    }

    pub(crate) fn rename(
        from: impl Into<String>,
        to: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        Self {
            identity: PackageIdentity::Rename {
                from: from.into(),
                to: to.into(),
            },
            version: PackageVersion::Current(version.into()),
            annotation: None,
            emphasized: false,
        }
    }

    pub(crate) fn annotated(mut self, annotation: impl Into<String>) -> Self {
        self.annotation = Some(annotation.into());
        self
    }

    pub(crate) fn emphasized(mut self, emphasized: bool) -> Self {
        self.emphasized = emphasized;
        self
    }

    fn sort_key(&self) -> &str {
        match &self.identity {
            PackageIdentity::Name(name) => name,
            PackageIdentity::Rename { to, .. } => to,
        }
    }
}

/// Human package-list grammar used by mutation plans, confirmations, results,
/// diagnostics, and flat query output. Styling may disappear when output is
/// redirected, but the textual structure remains unchanged.
pub(crate) fn render_section(action: &str, items: &[PackageListItem]) -> String {
    render_counted_section(action, "package", items)
}

pub(crate) fn render_counted_section(
    action: &str,
    noun: &str,
    items: &[PackageListItem],
) -> String {
    render_counted_section_with_total(action, noun, items.len(), items)
}

pub(crate) fn render_counted_section_with_total(
    action: &str,
    noun: &str,
    total: usize,
    items: &[PackageListItem],
) -> String {
    if items.is_empty() {
        return String::new();
    }
    let heading = format!("{action} {}:", glu_client::format::plural(total, noun));
    render(Some(&heading), true, items)
}

pub(crate) fn render_labeled_section(label: &str, items: &[PackageListItem]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let heading = format!(
        "{label} ({}):",
        glu_client::format::plural(items.len(), "package")
    );
    render(Some(&heading), true, items)
}

pub(crate) fn render_primitive(items: &[PackageListItem]) -> String {
    render(None, false, items)
}

pub(crate) fn print_section(action: &str, items: &[PackageListItem]) {
    let rendered = render_section(action, items);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
}

pub(crate) fn print_counted_section(action: &str, noun: &str, items: &[PackageListItem]) {
    let rendered = render_counted_section(action, noun, items);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
}

pub(crate) fn print_counted_section_with_total(
    action: &str,
    noun: &str,
    total: usize,
    items: &[PackageListItem],
) {
    let rendered = render_counted_section_with_total(action, noun, total, items);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
}

pub(crate) fn print_labeled_section(label: &str, items: &[PackageListItem]) {
    let rendered = render_labeled_section(label, items);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
}

pub(crate) fn print_primitive(items: &[PackageListItem]) {
    let rendered = render_primitive(items);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
}

fn render(heading: Option<&str>, marked: bool, items: &[PackageListItem]) -> String {
    let mut ordered: Vec<&PackageListItem> = items.iter().collect();
    ordered.sort_by(|a, b| {
        a.sort_key()
            .to_ascii_lowercase()
            .cmp(&b.sort_key().to_ascii_lowercase())
    });

    let mut lines = Vec::with_capacity(ordered.len() + usize::from(heading.is_some()));
    if let Some(heading) = heading {
        lines.push(heading.to_string());
    }
    for item in ordered {
        lines.push(render_item(item, marked));
    }
    lines.join("\n")
}

fn render_item(item: &PackageListItem, marked: bool) -> String {
    let mut line = if marked {
        format!("  {} ", style::dim("▪"))
    } else {
        String::new()
    };

    match &item.identity {
        PackageIdentity::Name(name) => {
            if item.emphasized {
                line.push_str(&style::bold(name));
            } else {
                line.push_str(name);
            }
        }
        PackageIdentity::Rename { from, to } => {
            line.push_str(from);
            line.push(' ');
            line.push_str(&style::dim("→"));
            line.push(' ');
            if item.emphasized {
                line.push_str(&style::bold(to));
            } else {
                line.push_str(to);
            }
        }
    }

    match &item.version {
        PackageVersion::None => {}
        PackageVersion::Current(version) => {
            line.push(' ');
            line.push_str(&style::dim(version));
        }
        PackageVersion::Change { from, to } => {
            line.push(' ');
            line.push_str(&style::dim(from));
            line.push(' ');
            line.push_str(&style::dim("→"));
            line.push(' ');
            line.push_str(&style::dim(to));
        }
    }
    if let Some(annotation) = &item.annotation {
        line.push(' ');
        line.push_str(&style::dim(&format!("({annotation})")));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_has_count_and_standard_markers() {
        let rendered = render_section(
            "Would install",
            &[
                PackageListItem::package("zlib", "1.3.1"),
                PackageListItem::package("curl", "8.11.1"),
            ],
        );
        assert_eq!(
            rendered,
            "Would install 2 packages:\n  ▪ curl 8.11.1\n  ▪ zlib 1.3.1"
        );
    }

    #[test]
    fn singular_and_version_changes_render_cleanly() {
        assert_eq!(
            render_section(
                "Would update",
                &[PackageListItem::update("node", "22.1.0", "22.2.0")]
            ),
            "Would update 1 package:\n  ▪ node 22.1.0 → 22.2.0"
        );
    }

    #[test]
    fn primitive_has_no_heading_or_marker() {
        assert_eq!(
            render_primitive(
                &[PackageListItem::package("node", "22.2.0").annotated("deactivated")]
            ),
            "node 22.2.0 (deactivated)"
        );
    }
}
