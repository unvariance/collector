use toml_edit::{value, DocumentMut, Item, Table};

pub fn ensure_version2(doc: &mut DocumentMut) -> bool {
    // Ensure top-level version = 2 if absent
    if !doc.contains_key("version") {
        doc["version"] = value(2);
        true
    } else {
        false
    }
}

pub fn ensure_nri_section(doc: &mut DocumentMut, socket_path: &str) -> bool {
    let mut changed = false;

    // Ensure [plugins] exists
    if !doc.as_table().contains_table("plugins") {
        doc["plugins"] = Item::Table(Table::new());
        changed = true;
    }

    // Ensure [plugins."io.containerd.nri.v1.nri"] exists (quoted key segment for dots)
    let plugins = doc["plugins"].as_table_mut().unwrap();
    if !plugins.contains_table("io.containerd.nri.v1.nri") {
        plugins.insert("io.containerd.nri.v1.nri", Item::Table(Table::new()));
        changed = true;
    }

    let t = plugins
        .get_mut("io.containerd.nri.v1.nri")
        .and_then(|i| i.as_table_mut())
        .unwrap();

    // Helper to set a default if missing
    // Required and defaults
    // Always enforce disable=false if not already false
    if t.get("disable")
        .and_then(|v| v.as_value())
        .map(|v| v.as_bool().unwrap_or(false))
        != Some(false)
    {
        t.insert("disable", value(false));
        changed = true;
    }
    if !t.contains_key("disable_connections") {
        t.insert("disable_connections", value(false));
        changed = true;
    }
    if !t.contains_key("plugin_config_path") {
        t.insert("plugin_config_path", value("/etc/nri/conf.d"));
        changed = true;
    }
    if !t.contains_key("plugin_path") {
        t.insert("plugin_path", value("/opt/nri/plugins"));
        changed = true;
    }
    if !t.contains_key("plugin_registration_timeout") {
        t.insert("plugin_registration_timeout", value("5s"));
        changed = true;
    }
    if !t.contains_key("plugin_request_timeout") {
        t.insert("plugin_request_timeout", value("2s"));
        changed = true;
    }

    // Ensure socket_path matches desired path
    let need_socket_update = t
        .get("socket_path")
        .and_then(|v| v.as_value())
        .and_then(|v| v.as_str())
        .map(|s| s != socket_path)
        .unwrap_or(true);
    if need_socket_update {
        t.insert("socket_path", value(socket_path));
        changed = true;
    }

    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_nri_to_minimal() {
        let mut d: DocumentMut = "".parse().unwrap();
        let mut changed = ensure_version2(&mut d);
        changed |= ensure_nri_section(&mut d, "/var/run/nri/nri.sock");
        assert!(changed);
        let s = d.to_string();
        assert!(s.contains("version = 2"));
        assert!(s.contains("plugins.\"io.containerd.nri.v1.nri\""));
        assert!(s.contains("disable = false"));
    }

    #[test]
    fn idempotent_add_twice() {
        let mut d: DocumentMut = "".parse().unwrap();
        let _ = ensure_version2(&mut d);
        let _ = ensure_nri_section(&mut d, "/var/run/nri/nri.sock");
        let first = d.to_string();
        let _ = ensure_version2(&mut d);
        let changed = ensure_nri_section(&mut d, "/var/run/nri/nri.sock");
        assert!(!changed);
        let second = d.to_string();
        assert_eq!(first, second);
    }
}
