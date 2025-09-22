# Unvariance Collector Helm Chart

This Helm chart deploys the Unvariance Collector, an eBPF-based tool for collecting high-resolution memory subsystem metrics in Kubernetes clusters.

## Installation

```bash
# Add the helm repository (if applicable)
helm repo add unvariance https://unvariance.github.io/collector/charts
helm repo update

# Install the chart with the default configuration
helm install collector unvariance/collector

# Install with custom configuration
helm install collector unvariance/collector -f your-values.yaml
```

## Configuration

### Deployment Modes

The Memory Collector supports different deployment modes:

1. **All Mode (Default)**: Deploy as a DaemonSet on all eligible nodes in the cluster.
   ```yaml
   deployment:
     mode: "all"
   ```

2. **Sample Mode**: Deploy as a Deployment with a specified number of replicas, ensuring they run on different nodes.
   ```yaml
   deployment:
     mode: "sample"
     sampleSize: 5  # Number of nodes to monitor
   ```

### Storage Options

Currently, the collector supports two storage types:

1. **S3 Storage**:
   ```yaml
   storage:
     type: "s3"
     prefix: "memory-collector-metrics-"
     s3:
       bucket: "your-bucket-name"
       region: "us-west-2"
       # For S3-compatible storage, specify the endpoint
       endpoint: "https://storage.googleapis.com"
       # For path-style URLs rather than virtual-hosted style
       pathStyle: false
       
       # Authentication options
       auth:
         method: "iam"  # Use IAM roles for service accounts
   ```

2. **Local Storage**:
   ```yaml
   storage:
     type: "local"
     prefix: "/tmp/memory-collector-metrics-"
   ```
   This type is not recommended for production use, only for testing. Files can be copied from the pod to the local machine using `kubectl cp`.

### Authentication Methods for S3

The chart supports three authentication methods for S3:

1. **IAM Roles for Service Accounts (IRSA)**:
   ```yaml
   serviceAccount:
     annotations:
       eks.amazonaws.com/role-arn: "arn:aws:iam::123456789012:role/S3Access"
   
   storage:
     s3:
       auth:
         method: "iam"
   ```

2. **Static Credentials**:
   ```yaml
   storage:
     s3:
       auth:
         method: "secret"
         accessKey: "YOUR_ACCESS_KEY"
         secretKey: "YOUR_SECRET_KEY"
   ```

3. **Existing Secret**:
   ```yaml
   storage:
     s3:
       auth:
         method: "existing"
         existingSecret: "my-s3-credentials"
         existingSecretKeyMapping:
           accessKey: "access_key_id"
           secretKey: "secret_access_key"
   ```

### Security Context and Capabilities

The Memory Collector requires certain Linux capabilities to interact with eBPF subsystems. By default, the chart uses a minimal non-privileged configuration:

```yaml
securityContext:
  privileged: false
  capabilities:
    add:
      - "BPF"
      - "PERFMON"
      - "SYS_RESOURCE"
  runAsUser: 0  # Required for eBPF operations
  # Optional: AppArmor profile for the collector container
  # Use Unconfined when enabling resctrl if your default AppArmor blocks sysfs writes
  appArmorProfile:
    type: Unconfined  # or RuntimeDefault | Localhost
    # localhostProfile: "my-loaded-profile"  # required only when type: Localhost
```

If you encounter issues with eBPF functionality, you may need to run in privileged mode:

```yaml
securityContext:
  privileged: true
```

To run on SELinux enabled systems, SELinux type and level must have
sufficient privileges to interact with eBPF. SELinux is by default enabled
on Fedora-based systems. See [Getting started with SELinux](https://docs.fedoraproject.org/en-US/quick-docs/selinux-getting-started/)
in the Fedora Documentation. The default configuration allows to run with
standard SELinux groups, and we recommend to keep it as is even on systems
where SELinux is not enabled.

More on [What is the spc_t container type, and why didn't we just run as unconfined_t?](https://danwalsh.livejournal.com/74754.html)

```yaml
podSecurityContext:
  seLinuxOptions:
    level: s0
    type: spc_t
```

On systems where SELinux is not enabled, the extra POD Security Context
options doesn't make any harm, but if you just would like to remove the
seLinuxOptions set podSecurityContext to an empty {}.

```yaml
podSecurityContext: {}  # Empty POD securityContext
## I have manually removed the seLinuxOptions as they are not relevant for
## our systems. 2025-09-22 I. N. Cognito
#  seLinuxOptions:
#    level: s0
#    type: spc_t
```

### Node Selection

You can customize which nodes the collector runs on using standard Kubernetes node selection:

```yaml
nodeSelector:
  kubernetes.io/os: linux
  node-role.kubernetes.io/worker: "true"

tolerations:
- key: "node-role.kubernetes.io/master"
  operator: "Equal"
  value: "true"
  effect: "NoSchedule"
```

### Resource Limits

Set resource limits for the collector pods:

```yaml
resources:
  limits:
    cpu: 200m
    memory: 256Mi
  requests:
    cpu: 100m
    memory: 128Mi
```

### NRI (Node Resource Interface) Configuration

The collector uses NRI to access pod and container metadata. NRI is disabled by default in containerd < 2.0. The chart includes an init container to check and optionally configure NRI:

```yaml
nri:
  configure: false # Default: detection-only (safest)
  restart: false   # Restart containerd to apply changes (may impact availability)
```

For detailed NRI setup instructions, see the [NRI Setup Guide](../../docs/nri-setup.md).

Recommended production rollout: use a rolling, label-based update to enable NRI safely in batches. See the NRI guide for step-by-step commands.

### Resctrl Collector (LLC Occupancy)

An optional feature that samples per-pod LLC occupancy via Linux resctrl and writes Parquet files.

Enable and configure via values:

```yaml
resctrl:
  enabled: false        # Disabled by default
  samplingInterval: "1s"
  mountpoint: "/sys/fs/resctrl"  # Host mount path for resctrl
  # Distinct filename/object prefix for resctrl outputs
  # Unlike the main collector stream (which uses `storage.prefix`),
  # resctrl files use this separate prefix to avoid mixing outputs.
  prefix: "resctrl-occupancy-"
  # If your nodes do not already have resctrl mounted, you can let the
  # chart mount it on the host with a small privileged initContainer.
  # This requires clusters that allow privileged pods and mount propagation.
  autoMountHost: false
  init:
    image:
      repository: busybox
      tag: "1.36"
      pullPolicy: IfNotPresent
    securityContext:
      privileged: true
      allowPrivilegeEscalation: true
      runAsUser: 0
```

Requirements when enabling resctrl:

- Writable mount of the host resctrl filesystem into the pod: the chart mounts
  `hostPath: /sys/fs/resctrl` at the same path inside the container with readOnly=false.
- Capabilities: creating resctrl monitor groups and assigning tasks typically
  requires root and `CAP_SYS_ADMIN`. You can either set `securityContext.privileged=true`
  (as you did in CI) or ensure `securityContext.capabilities.add` includes `SYS_ADMIN` and the
  pod runs as root (`runAsUser: 0`).
- AppArmor: If your nodes enforce an AppArmor profile that blocks writes under `/sys/fs/resctrl`,
  set `securityContext.appArmorProfile.type: Unconfined` for the collector container (or provide a
  permissive Localhost profile). On older clusters that do not support the appArmorProfile field,
  use a pod annotation: `container.apparmor.security.beta.kubernetes.io/collector: unconfined`.
- If you rely on the container to mount resctrl itself (plugin auto-mount), the
  default container runtime seccomp profile may still block the `mount(2)` call.
  Prefer pre-mounting resctrl on the node (e.g., via system configuration) or enable
  `resctrl.autoMountHost=true` to have the chart do a host mount via a privileged initContainer.


## Pod Security Standards Compatibility

The Memory Collector requires access to host resources and kernel facilities, which means it's not compatible with the "restricted" Pod Security Standard. It should be compatible with the "baseline" standard if running with the minimum required capabilities, or may require the "privileged" standard when run with privileged: true.

## Values Reference

| Parameter | Description | Default |
|-----------|-------------|---------|
| `nameOverride` | Override the name of the chart | `""` |
| `fullnameOverride` | Override the full name of the chart | `""` |
| `image.repository` | Image repository | `memory-collector` |
| `image.tag` | Image tag | `latest` |
| `image.pullPolicy` | Image pull policy | `IfNotPresent` |
| `deployment.mode` | Deployment mode: all, sample | `all` |
| `deployment.sampleSize` | Number of nodes to sample when in sample mode | `5` |
| `serviceAccount.create` | Create service account | `true` |
| `serviceAccount.name` | Service account name | `""` |
| `serviceAccount.annotations` | Service account annotations | `{}` |
| `securityContext.privileged` | Run container as privileged | `false` |
| `securityContext.capabilities.add` | Add capabilities to the container | `["BPF", "PERFMON", "SYS_RESOURCE", "SYS_ADMIN"]` |
| `securityContext.runAsUser` | User ID to run as | `0` |
| `securityContext.appArmorProfile` | AppArmor profile for the container (type: RuntimeDefault, Unconfined, Localhost) | `{}` |
| `collector.verbose` | Enable verbose debug output | `false` |
| `collector.duration` | Track duration in seconds (0 = unlimited) | `0` |
| `collector.trace` | Enable trace mode to output raw telemetry events at nanosecond granularity to parquet | `false` |
| `collector.parquetBufferSize` | Maximum memory buffer before flushing (bytes) | `104857600` |
| `collector.parquetFileSize` | Maximum Parquet file size (bytes) | `1073741824` |
| `collector.maxRowGroupSize` | Maximum row group size in Parquet | `1048576` |
| `collector.storageQuota` | Maximum total bytes to write to object store | `null` |
| `storage.type` | Storage type: local or s3 | `s3` |
| `storage.prefix` | Prefix for storage path | `memory-collector-metrics-` |
| `storage.s3.bucket` | S3 bucket name | `""` |
| `storage.s3.region` | S3 region | `""` |
| `storage.s3.endpoint` | S3 endpoint URL | `""` |
| `storage.s3.pathStyle` | Use path-style URLs | `false` |
| `storage.s3.auth.method` | Auth method: iam, secret, existing | `iam` |
| `storage.s3.auth.accessKey` | S3 access key for secret method | `""` |
| `storage.s3.auth.secretKey` | S3 secret key for secret method | `""` |
| `storage.s3.auth.existingSecret` | Existing secret name | `""` |
| `storage.s3.auth.existingSecretKeyMapping.accessKey` | Key in existing secret for access key | `access_key_id` |
| `storage.s3.auth.existingSecretKeyMapping.secretKey` | Key in existing secret for secret key | `secret_access_key` |
| `nodeSelector` | Node selectors | `{}` |
| `tolerations` | Node tolerations | `[]` |
| `affinity` | Node affinity rules | `{}` |
| `resources` | Pod resource requests and limits | See values.yaml |
| `podAnnotations` | Additional pod annotations | `{}` |
| `podLabels` | Additional pod labels | `{}` |
| `extraEnv` | Additional environment variables | `[]` |
| `nri.configure` | Configure NRI when socket is missing | `false` |
| `nri.restart` | Restart containerd to enable NRI | `false` |
| `nri.failIfUnavailable` | Fail init if NRI unavailable | `false` |
| `nri.init.image.repository` | Init image repository | `ghcr.io/unvariance/nri-init` |
| `nri.init.image.tag` | Init image tag | `latest` |
| `nri.init.image.pullPolicy` | Init image pull policy | `IfNotPresent` |
| `nri.init.command` | Init command | `["/bin/nri-init"]` |
| `nri.init.securityContext.privileged` | Run init as privileged | `true` |
| `nri.init.resources` | Init container resources | See values.yaml |
| `resctrl.enabled` | Enable resctrl LLC occupancy collector | `false` |
| `resctrl.samplingInterval` | Sampling interval for resctrl collector | `"1s"` |
| `resctrl.mountpoint` | Host mount path to mount in the pod | `"/sys/fs/resctrl"` |
| `resctrl.prefix` | Filename/object prefix for resctrl parquet outputs | `"resctrl-occupancy-"` |
