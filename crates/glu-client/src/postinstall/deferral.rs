// glu invention (deferral machinery): deterministic flush order for coalesced
// global steps (schemas → gio modules → fc-cache → gdk-pixbuf → icon cache →
// mime → desktop). The specific weights are glu-chosen; Homebrew has no
// ordering because it never defers.
pub(crate) fn global_flush_order(kind: &str) -> u16 {
    match kind {
        "compile_gsettings_schemas" => 10,
        "gio_querymodules" => 20,
        "fontconfig_fc_cache" => 25,
        "gdk_pixbuf_query_loaders" => 30,
        "gtk_update_icon_cache" => 40,
        "gtk_query_immodules_3" => 45,
        "update_mime_database" => 50,
        "update_desktop_database" => 60,
        _ => 1000,
    }
}

pub fn global_postinstall_label(kind: &str) -> String {
    match kind {
        "compile_gsettings_schemas" => "GSettings schema cache".to_string(),
        "gio_querymodules" => "GIO module cache".to_string(),
        "fontconfig_fc_cache" => "fontconfig cache".to_string(),
        "gdk_pixbuf_query_loaders" => "gdk-pixbuf loader cache".to_string(),
        "gtk_update_icon_cache" => "GTK icon cache".to_string(),
        "gtk_query_immodules_3" => "GTK 3 input module cache".to_string(),
        "update_mime_database" => "MIME database cache".to_string(),
        "update_desktop_database" => "desktop database cache".to_string(),
        _ => format!("{} cache", kind.replace('_', " ")),
    }
}
