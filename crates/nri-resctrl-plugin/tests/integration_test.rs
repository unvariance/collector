use std::time::Duration;

use tokio::sync::mpsc;

use nri::NRI;
use nri_resctrl_plugin::{PodResctrlEvent, ResctrlPlugin, ResctrlPluginConfig};

#[tokio::test]
#[ignore]
async fn test_resctrl_plugin_registers_with_nri() -> anyhow::Result<()> {
    // Use NRI socket path provided by the workflow via env.
    let socket_path = std::env::var("NRI_SOCKET_PATH")?;

    // Connect to NRI runtime socket
    let socket = tokio::net::UnixStream::connect(socket_path).await?;

    // Build plugin with an externally provided channel
    let (tx, mut _rx) = mpsc::channel::<PodResctrlEvent>(64);
    let plugin = ResctrlPlugin::new(ResctrlPluginConfig::default(), tx);

    // Start NRI server for plugin and register
    let (nri, join_handle) = NRI::new(socket, plugin, "resctrl-plugin", "10").await?;
    nri.register().await?;

    // Allow runtime to settle briefly
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Shut down cleanly
    nri.close().await?;
    join_handle.await??;

    Ok(())
}
