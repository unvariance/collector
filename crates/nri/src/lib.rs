// Include the generated code
include!(concat!(env!("OUT_DIR"), "/mod.rs"));

// Re-export the generated ttrpc code
pub mod api_ttrpc {
    include!(concat!(env!("OUT_DIR"), "/api_ttrpc.rs"));
}

pub mod events_mask;
pub mod metadata;
pub mod multiplex;

use anyhow::{anyhow, Result};
use log::info;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use ttrpc::context::Context;

use api::RegisterPluginRequest;
use api_ttrpc::{Plugin, RuntimeClient};

/// NRI struct provides a focused interface for NRI plugins
pub struct NRI {
    /// Plugin name
    plugin_name: String,
    /// Plugin index
    plugin_idx: String,
    /// Runtime client
    runtime_client: RuntimeClient,
    /// Shutdown channel sender
    shutdown_tx: mpsc::Sender<()>,
}

impl NRI {
    /// Create a new NRI instance and start the plugin server
    ///
    /// # Arguments
    ///
    /// * `socket` - Socket to connect to
    /// * `plugin` - Plugin implementation wrapped in `Arc`
    /// * `plugin_name` - Name of the plugin
    /// * `plugin_idx` - Index of the plugin (for ordering)
    ///
    /// # Returns
    ///
    /// * `Result<(NRI, JoinHandle<Result<()>>)>` - NRI instance and server task handle or error
    ///
    /// # Example
    ///
    /// ```no_run
    /// use nri::{NRI, metadata::MetadataPlugin};
    /// use tokio::sync::mpsc;
    ///
    /// #[tokio::main]
    /// async fn main() -> anyhow::Result<()> {
    ///     // Create a channel for metadata updates
    ///     let (tx, rx) = mpsc::channel(100);
    ///
    ///     // Create metadata plugin
    ///     let plugin = std::sync::Arc::new(MetadataPlugin::new(tx));
    ///
    ///     // Connect to the socket first
    ///     let socket = tokio::net::UnixStream::connect("/var/run/nri/nri.sock").await?;
    ///
    ///     // Create NRI instance and get join handle
    ///     let (nri, mut join_handle) = NRI::new(
    ///         socket,
    ///         plugin.clone(),
    ///         "metadata-plugin",
    ///         "10",
    ///     ).await?;
    ///
    ///     // Register the plugin with the runtime
    ///     nri.register().await?;
    ///
    ///     // Wait for the plugin to finish
    ///     join_handle.await??;
    ///
    ///     Ok(())
    /// }
    /// ```
    pub async fn new<P: Plugin + Send + Sync + 'static>(
        socket: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Sync + 'static,
        plugin: std::sync::Arc<P>,
        plugin_name: &str,
        plugin_idx: &str,
    ) -> Result<(Self, JoinHandle<Result<()>>)> {
        // Create the multiplexer using the socket
        let mut mux = multiplex::Mux::new(socket);

        // Open the runtime connection (client side)
        let rt_socket = mux.open(multiplex::RUNTIME_SERVICE_CONN).await?;
        let runtime_socket = ttrpc::r#async::transport::Socket::new(rt_socket);
        let runtime_client = RuntimeClient::new(ttrpc::r#async::Client::new(runtime_socket));

        // Create the plugin service (server side)
        let service_map = api_ttrpc::create_plugin(plugin);
        let mut server = ttrpc::r#async::Server::new().register_service(service_map);

        // Open plugin socket for the server
        let plugin_socket = mux.open(multiplex::PLUGIN_SERVICE_CONN).await?;
        let ttrpc_socket = ttrpc::r#async::transport::Socket::new(plugin_socket);

        // Create a shutdown channel
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

        // Start the server by spawning a task that takes ownership of the server and mux
        let join_handle = tokio::spawn(async move {
            info!("Starting NRI plugin server");

            // Create the server future
            let server_future = server.start_connected(ttrpc_socket);

            // Select between the three shutdown conditions
            let result = tokio::select! {
                // 1. Shutdown signal
                _ = shutdown_rx.recv() => {
                    info!("Received shutdown signal, stopping plugin server");
                    let _ = server.shutdown().await;
                    let _ = mux.shutdown().await;
                    Ok(())
                },
                // 2. Mux monitor handle returns (via channel)
                result = mux.monitor_handle() => {
                    info!("Multiplexer monitor returned, stopping plugin server");
                    let _ = server.shutdown().await;
                    match result {
                        Ok(Ok(())) => Ok(()),
                        Ok(Err(e)) => Err(anyhow!("Mux error: {}", e)),
                        Err(e) => Err(anyhow!("Monitor handle error: {}", e)),
                    }
                },
                // 3. TTRPC server future completes
                server_result = server_future => {
                    info!("TTRPC server future completed, stopping plugin server");
                    // Signal mux to shut down if it hasn't already
                    let _ = mux.shutdown().await;
                    server_result.map_err(|e| anyhow!("Server error: {}", e))
                }
            };

            info!("NRI plugin server stopped");
            result
        });

        let nri = Self {
            plugin_name: plugin_name.to_string(),
            plugin_idx: plugin_idx.to_string(),
            runtime_client,
            shutdown_tx,
        };

        Ok((nri, join_handle))
    }

    /// Register the plugin with the runtime
    ///
    /// This makes the RegisterPlugin RPC call to the runtime.
    ///
    /// # Returns
    ///
    /// * `Result<()>` - Success or error
    pub async fn register(&self) -> Result<()> {
        info!("Registering plugin '{}' with runtime", self.plugin_name);

        // Create the register request
        let req = RegisterPluginRequest {
            plugin_name: self.plugin_name.clone(),
            plugin_idx: self.plugin_idx.clone(),
            special_fields: protobuf::SpecialFields::default(),
        };

        // Make the RPC call
        self.runtime_client
            .register_plugin(Context::default(), &req)
            .await
            .map_err(|e| anyhow!("Registration error: {}", e))?;

        info!("Plugin '{}' registered successfully", self.plugin_name);
        Ok(())
    }

    /// Close the NRI connection and release resources
    ///
    /// This will signal the plugin server to shutdown and close the connection.
    ///
    /// # Returns
    ///
    /// * `Result<()>` - Success or error
    pub async fn close(&self) -> Result<()> {
        info!("Closing NRI connection");

        // Signal shutdown via the shutdown channel
        let _ = self.shutdown_tx.send(()).await;

        Ok(())
    }
}

// Export types for convenience
pub mod types {
    // NRI doesn't have all the types we were originally expecting
    // Export what's actually available from the generated code
    pub use crate::api::ContainerState;
    pub use crate::api::Event;
    pub use crate::api::LinuxNamespace;
    pub use crate::api::Mount;
    pub use crate::events_mask::{valid_events, EventMask};
}

// Include examples
#[cfg(feature = "examples")]
pub mod examples;

/// Compute the full cgroups path from container and pod information.
///
/// The container.linux.cgroups_path contains a colon-delimited string like:
/// "kubelet-kubepods-besteffort-podef89bdb6_d5d3_4396_9ed2_3a2006e0b6aa.slice:cri-containerd:cafbf51befe66f13ea3ece8780e7a7f711893d6fba12ddd5d689642fcdeba9b9"
///
/// The pod.linux.cgroup_parent contains the parent path like:
/// "/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-besteffort.slice/kubelet-kubepods-besteffort-podef89bdb6_d5d3_4396_9ed2_3a2006e0b6aa.slice"
/// or sometimes with the prefix already:
/// "/sys/fs/cgroup/kubelet.slice/kubelet-kubepods.slice/kubelet-kubepods-besteffort.slice/kubelet-kubepods-besteffort-podef89bdb6_d5d3_4396_9ed2_3a2006e0b6aa.slice"
///
/// We need to extract the second and third parts from the container path and combine them as:
/// "/sys/fs/cgroup" (if not present) + pod.linux.cgroup_parent + "/" + second_part + "-" + third_part + ".scope"
pub fn compute_full_cgroup_path(
    container: &api::Container,
    pod: Option<&api::PodSandbox>,
) -> String {
    // Get the container's cgroups path
    let container_cgroups_path = container
        .linux
        .as_ref()
        .map(|linux| linux.cgroups_path.as_str())
        .unwrap_or("");

    // Get the pod's cgroup parent
    let pod_cgroup_parent = pod
        .and_then(|p| p.linux.as_ref())
        .map(|linux| linux.cgroup_parent.as_str())
        .unwrap_or("");

    // Early return if there's no cgroup information at all
    if container_cgroups_path.is_empty() && pod_cgroup_parent.is_empty() {
        return String::new();
    }

    // Helper to ensure a path is rooted at /sys/fs/cgroup without duplicating slashes
    fn ensure_cgroup_prefix(path: &str) -> String {
        if path.starts_with("/sys/fs/cgroup") {
            path.to_string()
        } else if path.starts_with('/') {
            format!("/sys/fs/cgroup{}", path)
        } else {
            format!("/sys/fs/cgroup/{}", path)
        }
    }

    // Parse the container cgroups path (colon-delimited)
    let parts: Vec<&str> = container_cgroups_path.split(':').collect();

    // Preferred construction when we have both pod parent and container runtime/id
    if parts.len() >= 3 && !pod_cgroup_parent.is_empty() {
        let runtime = parts[1]; // e.g., "cri-containerd"
        let container_id = parts[2]; // e.g., "cafb..."
        let full_parent = ensure_cgroup_prefix(pod_cgroup_parent);

        // Detect cgroup hierarchy style:
        // - systemd driver: path contains ".slice" segments and containers are
        //   named like "<runtime>-<id>.scope"
        // - cgroupfs driver: path uses "kubepods/.../pod<uid>/<id>" without .slice
        if full_parent.contains(".slice") {
            return format!("{}/{}-{}.scope", full_parent, runtime, container_id);
        } else {
            return format!("{}/{}", full_parent, container_id);
        }
    }

    // Fallback: return container path (already absolute) with the cgroup prefix if missing
    ensure_cgroup_prefix(container_cgroups_path)
}
