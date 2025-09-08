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
```

If you encounter issues with eBPF functionality, you may need to run in privileged mode:

```yaml
securityContext:
  privileged: true
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
| `securityContext.capabilities.add` | Add capabilities to the container | `["BPF", "PERFMON", "SYS_RESOURCE"]` |
| `securityContext.runAsUser` | User ID to run as | `0` |
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
