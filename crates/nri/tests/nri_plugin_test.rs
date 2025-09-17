use anyhow::Result;
use nri::api::{
    ConfigureRequest, ConfigureResponse, CreateContainerRequest, CreateContainerResponse, Empty,
    Event, StateChangeEvent, StopContainerRequest, StopContainerResponse, SynchronizeRequest,
    SynchronizeResponse, UpdateContainerRequest, UpdateContainerResponse, UpdatePodSandboxRequest,
    UpdatePodSandboxResponse,
};
use nri::api_ttrpc::{Plugin, Runtime};
use nri::events_mask::EventMask;
use nri::multiplex::{Mux, RUNTIME_SERVICE_CONN};
use nri::NRI;
use protobuf::SpecialFields;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::timeout;
use ttrpc::context::Context;
use ttrpc::r#async::transport::Socket;
use ttrpc::r#async::TtrpcContext;

// A simple example plugin that counts method calls
struct CounterPlugin {
    configure_count: Arc<StdMutex<i32>>,
    synchronize_count: Arc<StdMutex<i32>>,
    create_container_count: Arc<StdMutex<i32>>,
    update_container_count: Arc<StdMutex<i32>>,
    stop_container_count: Arc<StdMutex<i32>>,
    update_pod_sandbox_count: Arc<StdMutex<i32>>,
    state_change_count: Arc<StdMutex<i32>>,
    shutdown_count: Arc<StdMutex<i32>>,
}

impl CounterPlugin {
    fn new() -> Self {
        Self {
            configure_count: Arc::new(StdMutex::new(0)),
            synchronize_count: Arc::new(StdMutex::new(0)),
            create_container_count: Arc::new(StdMutex::new(0)),
            update_container_count: Arc::new(StdMutex::new(0)),
            stop_container_count: Arc::new(StdMutex::new(0)),
            update_pod_sandbox_count: Arc::new(StdMutex::new(0)),
            state_change_count: Arc::new(StdMutex::new(0)),
            shutdown_count: Arc::new(StdMutex::new(0)),
        }
    }
}

impl Clone for CounterPlugin {
    fn clone(&self) -> Self {
        Self {
            configure_count: self.configure_count.clone(),
            synchronize_count: self.synchronize_count.clone(),
            create_container_count: self.create_container_count.clone(),
            update_container_count: self.update_container_count.clone(),
            stop_container_count: self.stop_container_count.clone(),
            update_pod_sandbox_count: self.update_pod_sandbox_count.clone(),
            state_change_count: self.state_change_count.clone(),
            shutdown_count: self.shutdown_count.clone(),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for CounterPlugin {
    async fn configure(
        &self,
        _ctx: &TtrpcContext,
        _req: ConfigureRequest,
    ) -> ttrpc::Result<ConfigureResponse> {
        // Increment counter
        {
            let mut count = self.configure_count.lock().unwrap();
            *count += 1;
        }

        // Return a response that subscribes to all container events
        let mut events = EventMask::new();
        events.set(&[
            Event::CREATE_CONTAINER,
            Event::UPDATE_CONTAINER,
            Event::STOP_CONTAINER,
            Event::UPDATE_POD_SANDBOX,
        ]);

        Ok(ConfigureResponse {
            events: events.raw_value(),
            special_fields: SpecialFields::default(),
        })
    }

    async fn synchronize(
        &self,
        _ctx: &TtrpcContext,
        _req: SynchronizeRequest,
    ) -> ttrpc::Result<SynchronizeResponse> {
        // Increment counter
        {
            let mut count = self.synchronize_count.lock().unwrap();
            *count += 1;
        }

        Ok(SynchronizeResponse::default())
    }

    async fn shutdown(&self, _ctx: &TtrpcContext, _req: Empty) -> ttrpc::Result<Empty> {
        // Increment counter
        {
            let mut count = self.shutdown_count.lock().unwrap();
            *count += 1;
        }

        Ok(Empty::default())
    }

    async fn create_container(
        &self,
        _ctx: &TtrpcContext,
        _req: CreateContainerRequest,
    ) -> ttrpc::Result<CreateContainerResponse> {
        // Increment counter
        {
            let mut count = self.create_container_count.lock().unwrap();
            *count += 1;
        }

        Ok(CreateContainerResponse::default())
    }

    async fn update_container(
        &self,
        _ctx: &TtrpcContext,
        _req: UpdateContainerRequest,
    ) -> ttrpc::Result<UpdateContainerResponse> {
        // Increment counter
        {
            let mut count = self.update_container_count.lock().unwrap();
            *count += 1;
        }

        Ok(UpdateContainerResponse::default())
    }

    async fn stop_container(
        &self,
        _ctx: &TtrpcContext,
        _req: StopContainerRequest,
    ) -> ttrpc::Result<StopContainerResponse> {
        // Increment counter
        {
            let mut count = self.stop_container_count.lock().unwrap();
            *count += 1;
        }

        Ok(StopContainerResponse::default())
    }

    async fn update_pod_sandbox(
        &self,
        _ctx: &TtrpcContext,
        _req: UpdatePodSandboxRequest,
    ) -> ttrpc::Result<UpdatePodSandboxResponse> {
        // Increment counter
        {
            let mut count = self.update_pod_sandbox_count.lock().unwrap();
            *count += 1;
        }

        Ok(UpdatePodSandboxResponse::default())
    }

    async fn state_change(
        &self,
        _ctx: &TtrpcContext,
        _req: StateChangeEvent,
    ) -> ttrpc::Result<Empty> {
        // Increment counter
        {
            let mut count = self.state_change_count.lock().unwrap();
            *count += 1;
        }

        Ok(Empty::default())
    }
}

// Mock Runtime service implementation for testing
#[derive(Clone)]
struct MockRuntimeService {
    // For tracking registration
    register_called: Arc<Mutex<bool>>,
    plugin_name: Arc<Mutex<String>>,
    plugin_idx: Arc<Mutex<String>>,
    plugin_client: Option<nri::api_ttrpc::PluginClient>,
}

impl MockRuntimeService {
    fn new() -> Self {
        Self {
            register_called: Arc::new(Mutex::new(false)),
            plugin_name: Arc::new(Mutex::new(String::new())),
            plugin_idx: Arc::new(Mutex::new(String::new())),
            plugin_client: None,
        }
    }

    async fn set_plugin_client(&mut self, client: nri::api_ttrpc::PluginClient) {
        self.plugin_client = Some(client);
    }

    async fn call_configure(&self) -> Result<ConfigureResponse> {
        if let Some(client) = &self.plugin_client {
            let req = ConfigureRequest::default();
            let resp = client.configure(Context::default(), &req).await?;
            Ok(resp)
        } else {
            Err(anyhow::anyhow!("Plugin client not set"))
        }
    }

    async fn call_synchronize(&self) -> Result<SynchronizeResponse> {
        if let Some(client) = &self.plugin_client {
            let req = SynchronizeRequest::default();
            let resp = client.synchronize(Context::default(), &req).await?;
            Ok(resp)
        } else {
            Err(anyhow::anyhow!("Plugin client not set"))
        }
    }

    async fn call_create_container(&self) -> Result<CreateContainerResponse> {
        if let Some(client) = &self.plugin_client {
            let req = CreateContainerRequest::default();
            let resp = client.create_container(Context::default(), &req).await?;
            Ok(resp)
        } else {
            Err(anyhow::anyhow!("Plugin client not set"))
        }
    }

    async fn call_update_container(&self) -> Result<UpdateContainerResponse> {
        if let Some(client) = &self.plugin_client {
            let req = UpdateContainerRequest::default();
            let resp = client.update_container(Context::default(), &req).await?;
            Ok(resp)
        } else {
            Err(anyhow::anyhow!("Plugin client not set"))
        }
    }

    async fn call_stop_container(&self) -> Result<StopContainerResponse> {
        if let Some(client) = &self.plugin_client {
            let req = StopContainerRequest::default();
            let resp = client.stop_container(Context::default(), &req).await?;
            Ok(resp)
        } else {
            Err(anyhow::anyhow!("Plugin client not set"))
        }
    }

    async fn call_update_pod_sandbox(&self) -> Result<UpdatePodSandboxResponse> {
        if let Some(client) = &self.plugin_client {
            let req = UpdatePodSandboxRequest::default();
            let resp = client.update_pod_sandbox(Context::default(), &req).await?;
            Ok(resp)
        } else {
            Err(anyhow::anyhow!("Plugin client not set"))
        }
    }

    async fn call_state_change(&self) -> Result<Empty> {
        if let Some(client) = &self.plugin_client {
            let req = StateChangeEvent::default();
            let resp = client.state_change(Context::default(), &req).await?;
            Ok(resp)
        } else {
            Err(anyhow::anyhow!("Plugin client not set"))
        }
    }
}

#[async_trait::async_trait]
impl Runtime for MockRuntimeService {
    async fn register_plugin(
        &self,
        _ctx: &TtrpcContext,
        req: nri::api::RegisterPluginRequest,
    ) -> ttrpc::Result<Empty> {
        // Record that the register function was called
        let mut register_called = self.register_called.lock().await;
        *register_called = true;

        // Store the plugin name and index for verification
        {
            let mut plugin_name = self.plugin_name.lock().await;
            *plugin_name = req.plugin_name.clone();
        }
        {
            let mut plugin_idx = self.plugin_idx.lock().await;
            *plugin_idx = req.plugin_idx.clone();
        }

        Ok(Empty::default())
    }

    async fn update_containers(
        &self,
        _ctx: &TtrpcContext,
        _req: nri::api::UpdateContainersRequest,
    ) -> ttrpc::Result<nri::api::UpdateContainersResponse> {
        Ok(nri::api::UpdateContainersResponse::default())
    }
}

#[tokio::test]
async fn test_nri_creation() -> Result<()> {
    // Create a duplex pipe for communication
    let (_runtime_stream, plugin_stream) = tokio::io::duplex(1024);

    // Create an NRI instance using CounterPlugin
    let plugin = std::sync::Arc::new(CounterPlugin::new());
    let (_nri, _join_handle) = NRI::new(plugin_stream, plugin, "test-plugin", "5").await?;

    Ok(())
}

#[tokio::test]
async fn test_counter_plugin_with_nri() -> Result<()> {
    // Create a duplex pipe for communication
    let (runtime_stream, plugin_stream) = tokio::io::duplex(1024);

    // Create the counter plugin
    let plugin = std::sync::Arc::new(CounterPlugin::new());

    // Save references to the counters
    let configure_count = plugin.configure_count.clone();
    let synchronize_count = plugin.synchronize_count.clone();
    let create_container_count = plugin.create_container_count.clone();
    let update_container_count = plugin.update_container_count.clone();
    let stop_container_count = plugin.stop_container_count.clone();
    let update_pod_sandbox_count = plugin.update_pod_sandbox_count.clone();
    let state_change_count = plugin.state_change_count.clone();

    // Create multiplexer for the runtime end
    let runtime_mux = Mux::new(runtime_stream);

    // Open the plugin connection
    let plugin_socket = runtime_mux.open(RUNTIME_SERVICE_CONN).await?;
    let ttrpc_socket = Socket::new(plugin_socket);

    // Create the mock runtime service
    let mut runtime_service = MockRuntimeService::new();

    // Create and start a ttrpc server for the runtime service
    let runtime_service_arc = Arc::new(runtime_service.clone());
    let service_map = nri::api_ttrpc::create_runtime(runtime_service_arc);
    let mut runtime_server = ttrpc::r#async::Server::new().register_service(service_map);

    // Start the server in a separate task
    let server_handle = tokio::spawn(async move {
        runtime_server
            .start_connected(ttrpc_socket)
            .await
            .map_err(|e| anyhow::anyhow!("Server error: {}", e))
    });

    // Create an NRI instance with the counter plugin
    let (nri, mut join_handle) =
        NRI::new(plugin_stream, plugin.clone(), "counter-plugin", "10").await?;

    // Register the plugin
    nri.register().await?;

    // Verify that the register function was called
    assert!(
        *runtime_service.register_called.lock().await,
        "Register function should have been called"
    );

    // Verify the plugin name and index were passed correctly
    assert_eq!(
        *runtime_service.plugin_name.lock().await,
        "counter-plugin",
        "Plugin name should match"
    );
    assert_eq!(
        *runtime_service.plugin_idx.lock().await,
        "10",
        "Plugin index should match"
    );

    // Verify no methods have been called yet
    assert_eq!(
        *configure_count.lock().unwrap(),
        0,
        "Configure should not have been called yet"
    );
    assert_eq!(
        *synchronize_count.lock().unwrap(),
        0,
        "Synchronize should not have been called yet"
    );

    // Get the plugin client from the multiplexer
    let plugin_client = {
        let plugin_socket = runtime_mux
            .open(nri::multiplex::PLUGIN_SERVICE_CONN)
            .await?;
        let ttrpc_socket = Socket::new(plugin_socket);
        let client = ttrpc::r#async::Client::new(ttrpc_socket);
        nri::api_ttrpc::PluginClient::new(client)
    };

    // Set the plugin client on the runtime service
    runtime_service.set_plugin_client(plugin_client).await;

    // Call configure and verify the count increased
    let configure_response = runtime_service.call_configure().await?;
    assert_eq!(
        *configure_count.lock().unwrap(),
        1,
        "Configure should have been called once"
    );

    // Verify the event subscription in the response
    let events = EventMask::from_raw(configure_response.events);
    assert!(
        events.is_set(Event::CREATE_CONTAINER),
        "Plugin should subscribe to CREATE_CONTAINER events"
    );
    assert!(
        events.is_set(Event::UPDATE_CONTAINER),
        "Plugin should subscribe to UPDATE_CONTAINER events"
    );
    assert!(
        events.is_set(Event::STOP_CONTAINER),
        "Plugin should subscribe to STOP_CONTAINER events"
    );
    assert!(
        events.is_set(Event::UPDATE_POD_SANDBOX),
        "Plugin should subscribe to UPDATE_POD_SANDBOX events"
    );

    // Call synchronize and verify the count increased
    runtime_service.call_synchronize().await?;
    assert_eq!(
        *synchronize_count.lock().unwrap(),
        1,
        "Synchronize should have been called once"
    );

    // Call create_container and verify the count increased
    runtime_service.call_create_container().await?;
    assert_eq!(
        *create_container_count.lock().unwrap(),
        1,
        "create_container should have been called once"
    );

    // Call update_container and verify the count increased
    runtime_service.call_update_container().await?;
    assert_eq!(
        *update_container_count.lock().unwrap(),
        1,
        "update_container should have been called once"
    );

    // Call stop_container and verify the count increased
    runtime_service.call_stop_container().await?;
    assert_eq!(
        *stop_container_count.lock().unwrap(),
        1,
        "stop_container should have been called once"
    );

    // Call update_pod_sandbox and verify the count increased
    runtime_service.call_update_pod_sandbox().await?;
    assert_eq!(
        *update_pod_sandbox_count.lock().unwrap(),
        1,
        "update_pod_sandbox should have been called once"
    );

    // Call state_change and verify the count increased
    runtime_service.call_state_change().await?;
    assert_eq!(
        *state_change_count.lock().unwrap(),
        1,
        "state_change should have been called once"
    );

    // Close the NRI connection
    nri.close().await?;

    // Wait for the plugin to shut down with timeout
    let _ = timeout(Duration::from_secs(1), &mut join_handle).await??;

    // Clean up
    server_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_nri_connection_error_handling() -> Result<()> {
    // Create a duplex pipe for communication
    let (runtime_stream, plugin_stream) = tokio::io::duplex(1024);

    // Create an NRI instance using CounterPlugin
    let plugin = std::sync::Arc::new(CounterPlugin::new());
    let (nri, mut join_handle) = NRI::new(plugin_stream, plugin, "test-plugin", "5").await?;

    // Close the runtime end of the connection to simulate a connection failure
    drop(runtime_stream);

    // we select nri.register() and the plugin_handle
    tokio::select! {
        result = nri.register() => {
            assert!(result.is_err(), "Register should fail when connection is closed");

            // Verify the error message contains information about the connection
            if let Err(e) = result {
                let error_string = e.to_string();
                assert!(
                    error_string.contains("Registration error") ||
                    error_string.contains("broken pipe") ||
                    error_string.contains("connection"),
                    "Error should indicate connection problem, got: {}", error_string
                );
            }
        }
        _ = &mut join_handle => {
            println!("Plugin handle finished");
        }
    }

    // Attempting to close should still succeed (idempotent)
    let close_result = nri.close().await;
    assert!(close_result.is_ok(), "Close should be idempotent");

    Ok(())
}
