use std::fs;
use std::path::PathBuf;
use tracing::info;

pub const DEFAULT_TEMPLATE_DIR: &str = "/var/lib/rancher/k3s/agent/etc/containerd";

const NRI_SECTION: &str = r#"

[plugins."io.containerd.nri.v1.nri"]
  disable = false
  disable_connections = false
  plugin_config_path = "/etc/nri/conf.d"
  plugin_path = "/opt/nri/plugins"
  plugin_registration_timeout = "5s"
  plugin_request_timeout = "2s"
  socket_path = "/var/run/nri/nri.sock"
"#;

pub fn configure_k3s_templates(dry_run: bool) -> std::io::Result<bool> {
    configure_k3s_templates_in(DEFAULT_TEMPLATE_DIR, dry_run)
}

pub fn configure_k3s_templates_in(base_dir: &str, dry_run: bool) -> std::io::Result<bool> {
    let mut changed = false;

    // Ensure template files exist; if not, create both with base header
    let template_v2 = PathBuf::from(base_dir).join("config.toml.tmpl");
    let template_v3 = PathBuf::from(base_dir).join("config-v3.toml.tmpl");
    let v2_exists = template_v2.exists();
    let v3_exists = template_v3.exists();
    if !v2_exists && !v3_exists {
        info!("No K3s containerd templates; creating both v2 and v3 templates");
        if !dry_run {
            fs::create_dir_all(base_dir)?;
            fs::write(
                &template_v2,
                "# K3s containerd config template with NRI\n{{ template \"base\" . }}\n",
            )?;
            fs::write(
                &template_v3,
                "# K3s containerd config template with NRI (v3)\n{{ template \"base\" . }}\n",
            )?;
        }
        changed = true;
    }

    // Ensure NRI section present in whichever templates exist
    for p in [&template_v2, &template_v3] {
        if p.exists() {
            let mut content = fs::read_to_string(p)?;
            if !content.contains("plugins.\"io.containerd.nri.v1.nri\"") {
                info!("Adding NRI section to {}", p.display());
                content.push_str(NRI_SECTION);
                if !dry_run {
                    fs::write(p, content)?;
                }
                changed = true;
            } else if content.contains("disable = true") {
                info!("Flipping disable=true to disable=false in {}", p.display());
                let newc = content.replace("disable = true", "disable = false");
                if newc != content {
                    if !dry_run {
                        fs::write(p, newc)?;
                    }
                    changed = true;
                }
            }
        }
    }

    Ok(changed)
}
