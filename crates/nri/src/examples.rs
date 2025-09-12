//! Examples showing how to use the NRI plugin API

use anyhow::Result;
use async_trait::async_trait;

use crate::api::{
    ConfigureRequest, ConfigureResponse, UpdateContainerRequest, UpdateContainerResponse,
    UpdatePodSandboxRequest, UpdatePodSandboxResponse,
};
use crate::api_ttrpc::Plugin;
use ttrpc::r#async::TtrpcContext; // Using the async context for our examples

/// Example of an NRI plugin implementation
pub struct ExamplePlugin;

#[async_trait]
impl Plugin for ExamplePlugin {
    async fn configure(
        &self,
        _ctx: &TtrpcContext,
        req: ConfigureRequest,
    ) -> ttrpc::Result<ConfigureResponse> {
        println!(
            "Received Configure request from runtime: {} {}",
            req.runtime_name, req.runtime_version
        );

        // Subscribe to no events by default in this simple example
        Ok(ConfigureResponse::default())
    }

    async fn update_container(
        &self,
        _ctx: &TtrpcContext,
        req: UpdateContainerRequest,
    ) -> ttrpc::Result<UpdateContainerResponse> {
        println!("Container update request received");

        // Print some information about the container if available
        if let Some(container) = req.container.as_ref() {
            println!("  Container ID: {}", container.id);
            println!("  Container name: {}", container.name);
            println!("  Pod ID: {}", container.pod_sandbox_id);
        }

        Ok(UpdateContainerResponse::default())
    }

    async fn update_pod_sandbox(
        &self,
        _ctx: &TtrpcContext,
        req: UpdatePodSandboxRequest,
    ) -> ttrpc::Result<UpdatePodSandboxResponse> {
        println!("Pod update request received");

        // Print some information about the pod if available
        if let Some(pod) = req.pod.as_ref() {
            println!("  Pod ID: {}", pod.id);
            println!("  Pod name: {}", pod.name);
            println!("  Pod namespace: {}", pod.namespace);
        }

        Ok(UpdatePodSandboxResponse::default())
    }
}

/// Example showing how to create and start a plugin using the high-level NRI helper
pub async fn example_plugin_server() -> Result<()> {
    // Connect to the NRI runtime socket
    let socket_path = "/var/run/nri/nri.sock";
    let stream = tokio::net::UnixStream::connect(socket_path).await?;

    // Create your plugin implementation
    let plugin = ExamplePlugin {};

    // Start the plugin server and get an NRI handle
    let (nri, _join) = crate::NRI::new(stream, plugin, "example-plugin", "10").await?;

    // Register with the runtime
    nri.register().await?;

    println!("Example plugin registered on {}", socket_path);

    // In a real application, you would keep running until shutdown
    Ok(())
}
