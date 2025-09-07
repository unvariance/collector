#!/bin/sh
set -e

# Script to check and optionally configure NRI for containerd
# Configuration is controlled by environment variables set from Helm values

# Default values
NRI_CONFIGURE="${NRI_CONFIGURE:-false}"
NRI_RESTART="${NRI_RESTART:-false}"
NRI_FAIL_IF_UNAVAILABLE="${NRI_FAIL_IF_UNAVAILABLE:-false}"
NRI_SOCKET_PATH="/var/run/nri/nri.sock"

# Function to log messages
log() {
    level=$1
    shift
    echo "[$(date '+%Y-%m-%d %H:%M:%S')] [$level] $*" >&2
}

# Detect if running on K3s
is_k3s() {
    if [ -d "/var/lib/rancher/k3s" ]; then
        log "INFO" "K3s installation detected"
        return 0
    else
        return 1
    fi
}

# Check if NRI socket exists and is functional
check_nri_socket() {
    if [ -S "$NRI_SOCKET_PATH" ]; then
        log "INFO" "NRI socket found at $NRI_SOCKET_PATH"
        
        # Also check if NRI is enabled in config (socket can exist even when disabled)
        config_file="/etc/containerd/config.toml"
        if [ -f "$config_file" ]; then
            if grep -q 'plugins."io.containerd.nri.v1.nri"' "$config_file"; then
                if grep -A 5 'plugins."io.containerd.nri.v1.nri"' "$config_file" | grep -q "disable = false"; then
                    log "INFO" "NRI is enabled in containerd config"
                    return 0
                else
                    log "WARN" "NRI socket exists but NRI is disabled in config"
                    return 1
                fi
            else
                log "WARN" "NRI socket exists but no NRI config section found"
                return 1
            fi
        fi
        
        # If K3s, check its config
        if is_k3s; then
            k3s_config="/var/lib/rancher/k3s/agent/etc/containerd/config.toml"
            if [ -f "$k3s_config" ] && grep -A 5 'plugins."io.containerd.nri.v1.nri"' "$k3s_config" | grep -q "disable = false"; then
                log "INFO" "NRI is enabled in K3s config"
                return 0
            fi
        fi
        
        # Socket exists but can't verify config - assume it's working
        log "INFO" "NRI socket exists, assuming it's functional"
        return 0
    else
        log "WARN" "NRI socket not found at $NRI_SOCKET_PATH"
        return 1
    fi
}

# Configure NRI for standard containerd
configure_containerd() {
    config_file="/etc/containerd/config.toml"
    
    log "INFO" "Configuring NRI for standard containerd at $config_file"
    
    # Check if containerd config exists
    if [ ! -f "$config_file" ]; then
        log "WARN" "Containerd config not found at $config_file, creating minimal config"
        mkdir -p /etc/containerd
        cat > "$config_file" <<EOF
version = 2
EOF
    fi
    
    # Check if NRI is already configured
    if grep -q 'plugins."io.containerd.nri.v1.nri"' "$config_file"; then
        log "INFO" "NRI section found in config, updating disable flag"
        # Use sed to update the disable flag with flexible whitespace
        sed -i 's/disable[[:space:]]*=[[:space:]]*true/disable = false/g' "$config_file"
    else
        log "INFO" "Adding NRI configuration to existing config"
        # Append NRI configuration
        cat >> "$config_file" <<EOF

[plugins."io.containerd.nri.v1.nri"]
  disable = false
  disable_connections = false
  plugin_config_path = "/etc/nri/conf.d"
  plugin_path = "/opt/nri/plugins"
  plugin_registration_timeout = "5s"
  plugin_request_timeout = "2s"
  socket_path = "$NRI_SOCKET_PATH"
EOF
    fi
    
    log "INFO" "Containerd configuration updated"
}

# Configure NRI for K3s
configure_k3s() {
    template_dir="/var/lib/rancher/k3s/agent/etc/containerd"
    template_v3="$template_dir/config-v3.toml.tmpl"
    template_v2="$template_dir/config.toml.tmpl"
    
    log "INFO" "Configuring NRI for K3s"
    
    # Check which template version to use
    # K3s with containerd 2.0 uses config-v3.toml.tmpl
    # K3s with containerd 1.7 and earlier uses config.toml.tmpl
    
    # First check if config-v3.toml.tmpl exists (newer K3s with containerd 2.0)
    if [ -f "$template_v3" ]; then
        template_file="$template_v3"
        log "INFO" "Found existing config-v3.toml.tmpl (containerd 2.0)"
    elif [ -f "$template_v2" ]; then
        template_file="$template_v2"
        log "INFO" "Found existing config.toml.tmpl (containerd 1.7 or earlier)"
    else
        # No template exists, create based on K3s version
        # Try to detect containerd version by checking K3s version
        # K3s v1.31.6+ and v1.32.2+ include containerd 2.0
        # For simplicity, we'll create both templates with the base template approach
        log "WARN" "No K3s containerd template found, creating templates"
        mkdir -p "$template_dir"
        
        # Create v2 template for older K3s versions
        template_file="$template_v2"
        cat > "$template_file" <<'EOF'
# K3s containerd config template with NRI enabled
# This extends the K3s base template and adds NRI configuration
{{ template "base" . }}
EOF
        log "INFO" "Created config.toml.tmpl for containerd 1.7 and earlier"
        
        # Also create v3 template for newer K3s versions
        cat > "$template_v3" <<'EOF'
# K3s containerd config template with NRI enabled (v3 format)
# This extends the K3s base template and adds NRI configuration
{{ template "base" . }}
EOF
        log "INFO" "Created config-v3.toml.tmpl for containerd 2.0"
    fi
    
    log "INFO" "Using template file: $template_file"
    
    # Check if NRI is already configured in template
    if grep -q 'plugins."io.containerd.nri.v1.nri"' "$template_file"; then
        log "INFO" "NRI section found in K3s template, updating disable flag"
        sed -i 's/disable[[:space:]]*=[[:space:]]*true/disable = false/g' "$template_file"
    else
        log "INFO" "Adding NRI configuration to K3s template"
        # Append NRI configuration to template
        cat >> "$template_file" <<'EOF'

[plugins."io.containerd.nri.v1.nri"]
  disable = false
  disable_connections = false
  plugin_config_path = "/etc/nri/conf.d"
  plugin_path = "/opt/nri/plugins"
  plugin_registration_timeout = "5s"
  plugin_request_timeout = "2s"
  socket_path = "/var/run/nri/nri.sock"
EOF
    fi
    
    # If we modified v2 template, also update v3 if it exists (and vice versa)
    if [ "$template_file" = "$template_v2" ] && [ -f "$template_v3" ]; then
        if ! grep -q 'plugins."io.containerd.nri.v1.nri"' "$template_v3"; then
            log "INFO" "Also updating config-v3.toml.tmpl for consistency"
            cat >> "$template_v3" <<'EOF'

[plugins."io.containerd.nri.v1.nri"]
  disable = false
  disable_connections = false
  plugin_config_path = "/etc/nri/conf.d"
  plugin_path = "/opt/nri/plugins"
  plugin_registration_timeout = "5s"
  plugin_request_timeout = "2s"
  socket_path = "/var/run/nri/nri.sock"
EOF
        fi
    elif [ "$template_file" = "$template_v3" ] && [ -f "$template_v2" ]; then
        if ! grep -q 'plugins."io.containerd.nri.v1.nri"' "$template_v2"; then
            log "INFO" "Also updating config.toml.tmpl for consistency"
            cat >> "$template_v2" <<'EOF'

[plugins."io.containerd.nri.v1.nri"]
  disable = false
  disable_connections = false
  plugin_config_path = "/etc/nri/conf.d"
  plugin_path = "/opt/nri/plugins"
  plugin_registration_timeout = "5s"
  plugin_request_timeout = "2s"
  socket_path = "/var/run/nri/nri.sock"
EOF
        fi
    fi
    
    log "INFO" "K3s containerd template(s) updated"
}

# Restart containerd service
restart_containerd() {
    # Try to use nsenter to execute commands in host namespace if available
    # This allows the init container to restart services on the host
    NSENTER=""
    if [ -e /host/proc/1/ns/mnt ]; then
        NSENTER="nsenter --target 1 --mount --uts --ipc --net --pid --"
        log "INFO" "Using nsenter to execute commands on host"
    fi
    
    # Capture timestamp before restart attempt
    restart_timestamp=$(date +%s)
    log "INFO" "Timestamp before restart attempt: $restart_timestamp"
    
    restart_issued=false
    service_name=""
    
    if is_k3s; then
        log "INFO" "Restarting K3s service to apply NRI configuration"
        # Try to restart K3s
        if [ -n "$NSENTER" ]; then
            # First check if systemctl or service is available
            if $NSENTER which systemctl >/dev/null 2>&1; then
                log "INFO" "Attempting K3s restart via systemctl"
                if $NSENTER systemctl restart k3s 2>/dev/null; then
                    log "INFO" "K3s service restart command issued via systemctl"
                    restart_issued=true
                    service_name="k3s"
                elif $NSENTER systemctl restart k3s-agent 2>/dev/null; then
                    log "INFO" "K3s-agent service restart command issued via systemctl"
                    restart_issued=true
                    service_name="k3s-agent"
                else
                    log "WARN" "Failed to restart K3s via systemctl"
                    log "WARN" "This may be due to container security restrictions"
                    log "INFO" "K3s configuration has been updated but requires manual restart"
                    return 2  # Special return code for restart attempted but failed
                fi
            elif $NSENTER which service >/dev/null 2>&1; then
                log "INFO" "Attempting K3s restart via service command"
                if $NSENTER service k3s restart 2>/dev/null; then
                    log "INFO" "K3s service restart command issued via service"
                    restart_issued=true
                    service_name="k3s"
                elif $NSENTER service k3s-agent restart 2>/dev/null; then
                    log "INFO" "K3s-agent service restart command issued via service"
                    restart_issued=true
                    service_name="k3s-agent"
                else
                    log "WARN" "Failed to restart K3s via service command"
                    log "WARN" "This may be due to container security restrictions"
                    log "INFO" "K3s configuration has been updated but requires manual restart"
                    return 2
                fi
            else
                log "WARN" "Neither systemctl nor service command available"
                log "INFO" "K3s configuration has been updated but requires manual restart"
                return 2
            fi
        else
            log "WARN" "Cannot restart K3s from container without nsenter"
            log "INFO" "K3s configuration has been updated but requires manual restart"
            return 2
        fi
    else
        log "INFO" "Restarting containerd service to apply NRI configuration"
        # Try to restart containerd
        if [ -n "$NSENTER" ]; then
            if $NSENTER which systemctl >/dev/null 2>&1; then
                log "INFO" "Attempting containerd restart via systemctl"
                if $NSENTER systemctl restart containerd 2>/dev/null; then
                    log "INFO" "Containerd service restart command issued via systemctl"
                    restart_issued=true
                    service_name="containerd"
                else
                    log "WARN" "Failed to restart containerd via systemctl"
                    log "WARN" "This may be due to container security restrictions"
                    log "INFO" "Containerd configuration has been updated but requires manual restart"
                    return 2
                fi
            elif $NSENTER which service >/dev/null 2>&1; then
                log "INFO" "Attempting containerd restart via service command"
                if $NSENTER service containerd restart 2>/dev/null; then
                    log "INFO" "Containerd service restart command issued via service"
                    restart_issued=true
                    service_name="containerd"
                else
                    log "WARN" "Failed to restart containerd via service command"
                    log "WARN" "This may be due to container security restrictions"
                    log "INFO" "Containerd configuration has been updated but requires manual restart"
                    return 2
                fi
            else
                log "WARN" "Neither systemctl nor service command available"
                log "INFO" "Containerd configuration has been updated but requires manual restart"
                return 2
            fi
        else
            log "WARN" "Cannot restart containerd from container without nsenter"
            log "INFO" "Containerd configuration has been updated but requires manual restart"
            return 2
        fi
    fi
    
    # If restart was issued, verify it actually happened
    if [ "$restart_issued" = "true" ]; then
        log "INFO" "Restart command was issued, verifying service actually restarted..."
        
        # Give service time to restart
        sleep 3
        
        # Try to verify restart by checking service start time
        restart_verified=false
        if [ -n "$NSENTER" ] && [ -n "$service_name" ]; then
            if $NSENTER which systemctl >/dev/null 2>&1; then
                # Try to get the service's active since timestamp
                active_since=$($NSENTER systemctl show "$service_name" --property=ActiveEnterTimestamp 2>/dev/null | cut -d= -f2-)
                if [ -n "$active_since" ]; then
                    # Convert to epoch timestamp if possible
                    if $NSENTER which date >/dev/null 2>&1; then
                        service_start_epoch=$($NSENTER date -d "$active_since" +%s 2>/dev/null || echo "0")
                        if [ "$service_start_epoch" -gt "$restart_timestamp" ]; then
                            log "INFO" "Service restart verified: $service_name restarted at $active_since"
                            restart_verified=true
                        else
                            log "WARN" "Service $service_name appears not to have restarted (started at: $active_since)"
                        fi
                    else
                        log "INFO" "Service active since: $active_since (unable to verify timestamp)"
                    fi
                fi
            fi
            
            # Alternative verification: check if process PID changed (if we can access it)
            if [ "$restart_verified" = "false" ]; then
                log "INFO" "Alternative verification: checking for service/process availability"
                if is_k3s && $NSENTER pgrep k3s >/dev/null 2>&1; then
                    log "INFO" "K3s process is running"
                elif ! is_k3s && $NSENTER pgrep containerd >/dev/null 2>&1; then
                    log "INFO" "Containerd process is running"
                fi
            fi
        fi
    fi
    
    log "INFO" "Waiting for NRI socket to become available..."
    for i in $(seq 1 30); do
        if [ -S "$NRI_SOCKET_PATH" ]; then
            log "INFO" "NRI socket is now available at $NRI_SOCKET_PATH"
            return 0
        fi
        # Check periodically with status updates
        if [ $((i % 5)) -eq 0 ]; then
            log "INFO" "Still waiting for NRI socket... ($i/30)"
        fi
        sleep 1
    done
    
    log "WARN" "NRI socket did not appear after restart within 30 seconds"
    log "INFO" "This may indicate that the service restart requires additional privileges"
    return 2
}

# Main execution
main() {
    log "INFO" "Starting NRI initialization check"
    log "INFO" "Configuration settings: NRI_CONFIGURE=$NRI_CONFIGURE, NRI_RESTART=$NRI_RESTART"
    
    # Check if NRI socket exists
    if check_nri_socket; then
        log "INFO" "NRI is already enabled and available"
        log "INFO" "Memory Collector can access pod and container metadata"
        exit 0
    fi
    
    # NRI socket doesn't exist
    log "WARN" "NRI is not currently enabled on this node"
    log "WARN" "Without NRI, the Memory Collector cannot access pod and container metadata"
    
    # Check if we should configure NRI
    if [ "$NRI_CONFIGURE" = "true" ]; then
        log "INFO" "Attempting to configure NRI for containerd"
        
        if is_k3s; then
            configure_k3s
        else
            configure_containerd
        fi
        
        # Check if we should restart containerd
        if [ "$NRI_RESTART" = "true" ]; then
            log "INFO" "Attempting to restart containerd/K3s to enable NRI"
            log "WARN" "This may temporarily affect container management operations"
            
            restart_containerd
            restart_result=$?
            
            if [ $restart_result -eq 0 ]; then
                log "INFO" "NRI successfully enabled"
                log "INFO" "Memory Collector can now access pod and container metadata"
            elif [ $restart_result -eq 2 ]; then
                log "INFO" "NRI configuration successfully updated"
                log "WARN" "Automatic restart not possible due to container security restrictions"
                log "INFO" "This is expected behavior in most Kubernetes environments"
                log "INFO" "To complete NRI enablement, restart containerd/K3s manually:"
                if is_k3s; then
                    log "INFO" "  sudo systemctl restart k3s  # or k3s-agent"
                else
                    log "INFO" "  sudo systemctl restart containerd"
                fi
                log "WARN" "Memory Collector will continue without metadata features until restart"
            else
                log "ERROR" "Failed to configure or restart containerd/K3s"
                log "WARN" "Memory Collector will continue without metadata features"
            fi
        else
            log "INFO" "NRI configuration updated but containerd not restarted"
            log "INFO" "To enable NRI, containerd must be restarted manually or during next maintenance"
            log "WARN" "Memory Collector will continue without metadata features until restart"
        fi
    else
        log "INFO" "NRI configuration is disabled (nri.configure=false)"
        log "INFO" "To enable NRI metadata collection:"
        log "INFO" "  1. Set nri.configure=true in Helm values"
        log "INFO" "  2. Optionally set nri.restart=true to restart containerd immediately"
        log "WARN" "Memory Collector will continue without metadata features"
    fi
    
    log "INFO" "NRI initialization check completed"

    # Final availability handling and exit
    if [ "$NRI_FAIL_IF_UNAVAILABLE" = "true" ]; then
        if check_nri_socket; then
            log "INFO" "NRI socket verified, exiting successfully"
            exit 0
        else
            log "ERROR" "NRI is not available and NRI_FAIL_IF_UNAVAILABLE=true"
            exit 1
        fi
    else
        # Re-check availability to avoid misleading logs
        if check_nri_socket; then
            log "INFO" "NRI is available; proceeding to start collector"
            exit 0
        else
            log "INFO" "Allowing collector to start despite NRI unavailability"
            exit 0
        fi
    fi
}

# Run main function
main
