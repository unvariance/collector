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
    template_file="/var/lib/rancher/k3s/agent/etc/containerd/config.toml.tmpl"
    
    log "INFO" "Configuring NRI for K3s at $template_file"
    
    # Check if template exists
    if [ ! -f "$template_file" ]; then
        log "WARN" "K3s containerd template not found at $template_file, creating one"
        mkdir -p /var/lib/rancher/k3s/agent/etc/containerd
        cat > "$template_file" <<'EOF'
# K3s containerd config template with NRI enabled
version = 2

[plugins."io.containerd.grpc.v1.cri"]
  enable_selinux = {{ .NodeConfig.SELinux }}
  enable_unprivileged_ports = {{ .EnableUnprivilegedPorts }}
  enable_unprivileged_icmp = {{ .EnableUnprivilegedICMP }}

[plugins."io.containerd.grpc.v1.cri".containerd]
  snapshotter = "{{ .NodeConfig.AgentConfig.Snapshotter }}"
  disable_snapshot_annotations = {{ .DisableSnapshotAnnotations }}
  discard_unpacked_layers = {{ .DiscardUnpackedLayers }}

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc]
  runtime_type = "io.containerd.runc.v2"

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.runc.options]
  SystemdCgroup = {{ .SystemdCgroup }}
EOF
    fi
    
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
    
    log "INFO" "K3s containerd template updated"
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
    
    if is_k3s; then
        log "INFO" "Restarting K3s service to apply NRI configuration"
        # Try to restart K3s
        if [ -n "$NSENTER" ]; then
            # First check if systemctl or service is available
            if $NSENTER which systemctl >/dev/null 2>&1; then
                log "INFO" "Attempting K3s restart via systemctl"
                if $NSENTER systemctl restart k3s 2>/dev/null; then
                    log "INFO" "K3s service restart command issued via systemctl"
                elif $NSENTER systemctl restart k3s-agent 2>/dev/null; then
                    log "INFO" "K3s-agent service restart command issued via systemctl"
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
                elif $NSENTER service k3s-agent restart 2>/dev/null; then
                    log "INFO" "K3s-agent service restart command issued via service"
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
    
    # Check if we should fail when NRI is unavailable
    if [ "$NRI_FAIL_IF_UNAVAILABLE" = "true" ]; then
        # Check one more time if NRI socket exists after all operations
        if check_nri_socket; then
            log "INFO" "NRI socket verified, exiting successfully"
            exit 0
        else
            log "ERROR" "NRI is not available and NRI_FAIL_IF_UNAVAILABLE=true"
            exit 1
        fi
    else
        # Always exit successfully to allow collector to run
        log "INFO" "Allowing collector to start despite NRI unavailability"
        exit 0
    fi
}

# Run main function
main