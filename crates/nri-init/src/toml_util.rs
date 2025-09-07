use toml_edit::{DocumentMut, value};

pub const NRI_TABLE: &str = "plugins.\"io.containerd.nri.v1.nri\"";

pub fn ensure_version2(doc: &mut DocumentMut) -> bool {
    // Ensure top-level version = 2 if absent
    if !doc.contains_key("version") {
        doc["version"] = value(2);
        true
    } else { false }
}

pub fn ensure_nri_section(doc: &mut DocumentMut, socket_path: &str) -> bool {
    let mut changed = false;
    if !doc.as_table().contains_table(NRI_TABLE) {
        // Create the table
        let table = doc.as_table_mut().entry(NRI_TABLE).or_insert(toml_edit::Item::Table(toml_edit::Table::new()));
        let t = table.as_table_mut().unwrap();
        t.insert("disable", value(false));
        t.insert("disable_connections", value(false));
        t.insert("plugin_config_path", value("/etc/nri/conf.d"));
        t.insert("plugin_path", value("/opt/nri/plugins"));
        t.insert("plugin_registration_timeout", value("5s"));
        t.insert("plugin_request_timeout", value("2s"));
        t.insert("socket_path", value(socket_path));
        changed = true;
    } else {
        // Ensure disable = false only
        if let Some(t) = doc.as_table_mut().get_mut(NRI_TABLE).and_then(|i| i.as_table_mut()) {
            if t.get("disable").and_then(|v| v.as_value()).map(|v| v.as_bool().unwrap_or(false)) != Some(false) {
                t.insert("disable", value(false));
                changed = true;
            }
        }
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

