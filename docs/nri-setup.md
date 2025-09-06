# NRI (Node Resource Interface) Setup Guide

## Overview

The Memory Collector uses Node Resource Interface (NRI) to access pod and container metadata in Kubernetes. This guide explains how to enable NRI support for the collector.

## Background

NRI is disabled by default in containerd versions before 2.0, which affects most current Kubernetes distributions:

- **K3s**: Ships with containerd 1.7.x (NRI disabled)
- **Ubuntu LTS**: 24.04, 22.04, 20.04 ship containerd 1.7.x or older (NRI disabled)
- **Cloud Providers**: Most managed Kubernetes services use containerd 1.7.x (NRI disabled)

Without NRI enabled, the collector cannot access pod and container metadata, limiting its ability to correlate performance metrics with specific workloads.

## How the NRI Init Container Works

The collector Helm chart includes an init container that:

1. **Checks NRI availability**: Looks for the NRI socket at `/var/run/nri/nri.sock`
2. **Reports status**: Logs whether NRI is enabled or disabled
3. **Configures containerd** (optional): Updates containerd configuration to enable NRI
4. **Restarts containerd** (optional): Applies the configuration by restarting the service

## Configuration Options

The NRI feature is controlled by Helm values:

```yaml
nri:
  configure: true   # Update containerd config when NRI is disabled (default: true)
  restart: false    # Restart containerd to apply changes (default: false)
```

### Operating Modes

#### 1. Detection Only (Safest)
```bash
helm install collector ./charts/collector \
  --set nri.configure=false \
  --set nri.restart=false
```
- Only checks and reports NRI status
- No changes to the system
- Collector runs without metadata features

#### 2. Configure Without Restart (Default)
```bash
helm install collector ./charts/collector
# Or explicitly:
helm install collector ./charts/collector \
  --set nri.configure=true \
  --set nri.restart=false
```
- Updates containerd configuration
- Does NOT restart containerd
- Prepares nodes for manual restart during maintenance

#### 3. Full Setup with Restart
```bash
helm install collector ./charts/collector \
  --set nri.configure=true \
  --set nri.restart=true
```
- Updates containerd configuration
- Restarts containerd immediately
- **WARNING**: May temporarily disrupt container management

## Production Deployment Strategy

### Recommended Approach

1. **Initial Deployment**: Deploy with configuration only (default)
   ```bash
   helm install collector ./charts/collector
   ```
   This prepares all nodes without disruption.

2. **Check Status**: Review init container logs to verify configuration
   ```bash
   kubectl logs -n <namespace> <collector-pod> -c nri-init
   ```

3. **Schedule Maintenance**: During a maintenance window, enable restart
   ```bash
   helm upgrade collector ./charts/collector \
     --set nri.restart=true
   ```

4. **Verify NRI**: Check that NRI is enabled
   ```bash
   kubectl exec -n <namespace> <collector-pod> -- ls -la /var/run/nri/nri.sock
   ```

### Rolling Update Strategy

For large clusters, consider updating nodes gradually:

1. **Label nodes** for staged rollout:
   ```bash
   kubectl label node node1 nri-update=batch1
   kubectl label node node2 nri-update=batch2
   ```

2. **Update by batch** using node selectors:
   ```bash
   helm upgrade collector ./charts/collector \
     --set nri.restart=true \
     --set nodeSelector.nri-update=batch1
   ```

3. **Verify** each batch before proceeding:
   ```bash
   kubectl get pods -o wide | grep collector
   ```

## Troubleshooting

### Common Issues

#### NRI Socket Not Found After Restart
```bash
# Check containerd status
systemctl status containerd

# Check configuration was applied
grep -A5 "nri" /etc/containerd/config.toml

# For K3s, check template
grep -A5 "nri" /var/lib/rancher/k3s/agent/etc/containerd/config.toml.tmpl
```

#### Init Container Fails
```bash
# View init container logs
kubectl logs <pod> -c nri-init

# Check permissions
kubectl describe pod <pod>
```

#### Containerd Restart Impact
If containerd restart causes issues:
1. Kubelet may temporarily lose connection to containers
2. Existing containers continue running
3. New pod scheduling may be delayed
4. Recovery typically takes 30-60 seconds

### Manual NRI Setup

If you prefer manual configuration:

1. **Edit containerd config**:
   ```toml
   # /etc/containerd/config.toml
   [plugins."io.containerd.nri.v1.nri"]
     disable = false
     socket_path = "/var/run/nri/nri.sock"
   ```

2. **Restart containerd**:
   ```bash
   systemctl restart containerd
   ```

3. **Deploy collector** without NRI configuration:
   ```bash
   helm install collector ./charts/collector \
     --set nri.configure=false
   ```

## Monitoring NRI Status

### Check Logs
```bash
# View all collector pods' init container logs
kubectl logs -l app.kubernetes.io/name=collector -c nri-init
```

### Expected Log Messages

**NRI Enabled**:
```
[INFO] NRI socket found at /var/run/nri/nri.sock
[INFO] NRI is already enabled and available
[INFO] Memory Collector can access pod and container metadata
```

**NRI Disabled (configure=false)**:
```
[WARN] NRI socket not found at /var/run/nri/nri.sock
[INFO] NRI configuration is disabled (nri.configure=false)
[WARN] Memory Collector will continue without metadata features
```

**NRI Configured (restart=false)**:
```
[INFO] NRI configuration updated but containerd not restarted
[INFO] To enable NRI, containerd must be restarted manually
[WARN] Memory Collector will continue without metadata features until restart
```

**NRI Enabled After Restart**:
```
[INFO] Restarting containerd service to apply NRI configuration
[INFO] NRI socket is now available at /var/run/nri/nri.sock
[INFO] Memory Collector can now access pod and container metadata
```

## Security Considerations

The init container requires privileged access to:
- Modify containerd configuration files
- Restart system services (when enabled)
- Access host filesystem paths

These permissions are only used during initialization and are not required by the main collector container.

## Future Improvements

- **Containerd 2.0**: NRI is enabled by default (no configuration needed)
- **Automated detection**: Future versions may auto-detect safe restart windows
- **Gradual rollout**: Built-in support for phased node updates