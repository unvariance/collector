# NRI (Node Resource Interface) Setup Guide

## Overview

The Memory Collector uses Node Resource Interface (NRI) to access pod and container metadata in Kubernetes. This guide explains how to enable NRI support for the collector.

## Background

NRI support in containerd begins with containerd 1.7 (initially experimental) and is enabled-by-default starting with containerd 2.0. This has important implications for environments on older runtimes:

- NOTE: containerd 1.6.x and earlier do not include the NRI plugin at all. No configuration can enable NRI on 1.6.x — you must upgrade to containerd >= 1.7 to use NRI.

NRI is disabled by default in containerd 1.7.x, which affects many Kubernetes distributions:

- **K3s**: See the K3s version matrix below (some releases use containerd 2.0 with NRI enabled by default; older lines use 1.7.x with NRI disabled)
- **Ubuntu LTS**: 24.04, 22.04, 20.04 ship containerd 1.7.x or older (NRI disabled)
- **Cloud Providers**: Most managed Kubernetes services use containerd 1.7.x (NRI disabled)

Without NRI enabled, the collector cannot access pod and container metadata, limiting its ability to correlate performance metrics with specific workloads.

If the init container detects containerd < 1.7, it skips NRI configuration with a clear warning. Set `nri.failIfUnavailable=true` if you prefer the pod to fail fast in that case.

### K3s Versions and NRI

K3s adopted containerd 2.0 (which enables NRI by default) beginning in February 2025 for specific release lines. Earlier versions continue to use containerd 1.7.x (where NRI is disabled by default).

- NRI enabled by default (via containerd 2.0):
  - K3s `v1.31.6+` (starting with `v1.31.6+k3s1`)
  - K3s `v1.32.2+` (starting with `v1.32.2+k3s1`)
- No backports to older minors: the `v1.30.x`, `v1.29.x`, and `v1.28.x` release lines remain on containerd 1.7.x (NRI disabled by default).
- Prior to the above cutovers (e.g., K3s `v1.31.0–v1.31.5`, `v1.32.0–v1.32.1`), containerd 1.7.x is used and NRI must be explicitly enabled.

What this means for K3s clusters:

- If your cluster runs one of the versions with containerd 2.0 (see above), NRI is already enabled by default and no configuration is required.
- If your cluster runs any other supported K3s version (including all of `v1.30.x` and older minors, and early `v1.31`/`v1.32` patch releases), you need to enable NRI explicitly (use this guide or upgrade to a containerd 2.0-based K3s patch).

### KIND Versions and NRI

NRI status by KIND release:

- KIND `v0.27.0+`: NRI enabled by default (via containerd 2.x)
- KIND `v0.26.0` and earlier: NRI not enabled by default

Note: Unlike K3s (which adopted containerd 2.0 only in select release lines), each KIND release supports multiple Kubernetes versions. When KIND `v0.27.0` adopted containerd 2.0, it applied across all Kubernetes versions you can deploy with that KIND release.

In summary: To get NRI enabled by default in KIND, use `v0.27.0` or later.

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
  configure: false         # Default: detection-only (safest)
  restart: false           # Restart containerd to apply changes
  failIfUnavailable: false # Fail pod if NRI cannot be enabled/detected
  init:
    image:
      repository: ghcr.io/unvariance/nri-init
      tag: latest
      pullPolicy: IfNotPresent
    command: ["/bin/nri-init"]
    securityContext:
      privileged: true
    resources:
      limits:
        cpu: 200m
        memory: 128Mi
```

### Operating Modes

#### 1. Detection Only (Default, Safest)
```bash
helm install collector ./charts/collector
```
- Checks and reports NRI status only
- No system changes; collector runs without metadata if NRI is disabled

Optional: fail fast when NRI is unavailable
```bash
helm install collector ./charts/collector \
  --set nri.failIfUnavailable=true
```

#### 2. Configure Without Restart
```bash
helm install collector ./charts/collector \
  --set nri.configure=true \
  --set nri.restart=false
```
- Updates containerd configuration
- Does NOT restart containerd
- Prepares nodes for manual restart during maintenance

The chart uses an init container binary. You can override the image/tag if needed:
```bash
helm install collector ./charts/collector \
  --set nri.init.image.repository=ghcr.io/unvariance/nri-init \
  --set nri.init.image.tag=latest \
  --set nri.init.image.pullPolicy=IfNotPresent
```

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

### Recommended: Rolling Update (Safest)

1. **Initial Deploy (Default)**: Detection-only to assess environment
   ```bash
   helm install collector ./charts/collector
   ```
   Review init logs to confirm status:
   ```bash
   kubectl logs -n <namespace> <collector-pod> -c nri-init
   ```

2. **Label a first batch of nodes**
   ```bash
   kubectl label node node1 nri-update=batch1
   kubectl label node node2 nri-update=batch1
   ```

3. **Enable NRI for batch1** (configure and restart only on labeled nodes)
   ```bash
   helm upgrade collector ./charts/collector \
     --set nri.configure=true \
     --set nri.restart=true \
     --set nodeSelector.nri-update=batch1
   ```

4. **Verify NRI** on batch1 before proceeding
   ```bash
   kubectl get pods -o wide | grep collector
   kubectl exec -n <namespace> <collector-pod-on-batch1> -- ls -la /var/run/nri/nri.sock
   ```

5. **Roll forward**: relabel next set (e.g., batch2) and repeat step 3
   ```bash
   kubectl label node node3 nri-update=batch2
   helm upgrade collector ./charts/collector \
     --set nri.configure=true \
     --set nri.restart=true \
     --set nodeSelector.nri-update=batch2
   ```

6. **Finish**: when all nodes are updated, optionally remove the selector to return to normal scheduling
   ```bash
   helm upgrade collector ./charts/collector --reuse-values --set nodeSelector={}
   ```

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

These examples reflect the init binary's actual log strings.

**NRI Enabled**:
```
INFO Starting NRI initialization check
INFO Configuration: configure=false, restart=false
INFO NRI socket found at /var/run/nri/nri.sock
INFO NRI configuration is disabled (configure=false)
INFO Completed: configured=false, restarted=false, socket=true
INFO nri-init done
```

**NRI Disabled (configure=false)**:
```
INFO Starting NRI initialization check
INFO Configuration: configure=false, restart=false
WARN NRI socket not found at /var/run/nri/nri.sock
INFO NRI configuration is disabled (configure=false)
INFO Completed: configured=false, restarted=false, socket=false
INFO nri-init done
```

**NRI Configured (restart=false)**:
```
INFO Starting NRI initialization check
INFO Configuration: configure=true, restart=false
WARN NRI socket not found at /var/run/nri/nri.sock
INFO Updating containerd NRI configuration at /etc/containerd/config.toml
INFO Completed: configured=true, restarted=false, socket=false
INFO nri-init done
```

Note: If the configuration is already present, you may see:
```
INFO Containerd NRI configuration already up to date
```

**NRI Enabled After Restart**:
```
INFO Starting NRI initialization check
INFO Configuration: configure=true, restart=true
WARN NRI socket not found at /var/run/nri/nri.sock
INFO Updating containerd NRI configuration at /etc/containerd/config.toml
INFO Issued restart via systemctl for containerd
INFO NRI socket became available
INFO Completed: configured=true, restarted=true, socket=true
INFO nri-init done
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
