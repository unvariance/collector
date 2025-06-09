# NRI Integration Tests

This directory contains integration tests for the NRI (Node Resource Interface) implementation.

## Running the Integration Tests

### Prerequisites

- A Kubernetes cluster with NRI enabled (e.g., containerd with NRI plugin support)
- kubectl configured to access the cluster
- The NRI socket available at `/var/run/nri/nri.sock` or specified via the `NRI_SOCKET_PATH` environment variable

### Running Tests

Most tests can be run with the standard cargo test command:

```bash
cargo test
```

However, the integration test that connects to a real Kubernetes cluster is ignored by default because it requires:
1. A running Kubernetes cluster
2. Access to the NRI socket
3. Permission to create and delete pods

To run the integration test:

```bash
# Run the ignored integration test
cargo test --test integration_test -- --ignored

# With a custom NRI socket path
NRI_SOCKET_PATH=/path/to/nri.sock cargo test --test integration_test -- --ignored
```

## Test Description

The integration test (`integration_test.rs`) verifies:

1. Connection to the NRI socket
2. Registration of the metadata plugin
3. Collection of pre-existing container metadata
4. Creation of a new test pod and verification of its metadata
5. Deletion of both the pre-existing test pod and the new test pod and verification of container removal events

This ensures that the metadata plugin correctly integrates with both the NRI runtime and the Kubernetes API. 