use nri_init::LogLevel;
use tracing_subscriber::EnvFilter;

fn init_tracing(level: LogLevel) {
    let filter = match level {
        LogLevel::Error => "error",
        LogLevel::Warn => "warn",
        LogLevel::Info => "info",
        LogLevel::Debug => "debug",
        LogLevel::Trace => "trace",
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter))
        .with_target(false)
        .try_init();
}

fn main() {
    let opts = nri_init::opts::from_env_and_args();
    init_tracing(opts.log_level);
    match nri_init::run(opts) {
        Ok(out) => {
            tracing::info!(
                "Completed: configured={}, restarted={}, socket={}",
                out.configured,
                out.restarted,
                out.socket_available
            );
            tracing::info!("nri-init done");
            std::process::exit(0);
        }
        Err(e) => {
            tracing::error!("nri-init failed: {e}");
            std::process::exit(1);
        }
    }
}
