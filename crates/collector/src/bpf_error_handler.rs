use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use log::error;
use tokio::sync::mpsc;
use tokio::time;

use bpf::{msg_type, BpfLoader, TimerMigrationMsg};
use plain;

/// Event types that can be sent to the error reporting channel
#[derive(Debug)]
pub enum ErrorEvent {
    LostEvents,
}

/// BPF Error Handler manages error-related BPF events like timer migration and lost samples
pub struct BpfErrorHandler {
    error_sender: Option<mpsc::Sender<ErrorEvent>>,
    error_receiver: Option<mpsc::Receiver<ErrorEvent>>,
}

impl BpfErrorHandler {
    /// Create a new BpfErrorHandler and subscribe to error events
    pub fn new(bpf_loader: &mut BpfLoader) -> Rc<RefCell<Self>> {
        let (sender, receiver) = mpsc::channel(1000);
        let handler = Rc::new(RefCell::new(Self {
            error_sender: Some(sender),
            error_receiver: Some(receiver),
        }));

        // Subscribe to timer migration events
        let dispatcher = bpf_loader.dispatcher_mut();
        dispatcher.subscribe_method(
            msg_type::MSG_TYPE_TIMER_MIGRATION_DETECTED as u32,
            handler.clone(),
            BpfErrorHandler::handle_timer_migration,
        );

        // Subscribe to lost samples events
        let handler_clone = handler.clone();
        dispatcher.subscribe_lost_samples(move |ring_index, data| {
            handler_clone.borrow().handle_lost_events(ring_index, data);
        });

        handler
    }

    /// Take the receiver for running the error reporting task
    pub fn take_receiver(&mut self) -> Option<mpsc::Receiver<ErrorEvent>> {
        self.error_receiver.take()
    }

    /// Run the error reporting task that batches and reports errors
    pub async fn run_error_reporting(mut receiver: mpsc::Receiver<ErrorEvent>) {
        let mut interval = time::interval(Duration::from_secs(1));
        let mut lost_events_count = 0;

        loop {
            tokio::select! {
                // Check for new error events
                event = receiver.recv() => {
                    match event {
                        Some(ErrorEvent::LostEvents) => {
                            lost_events_count += 1;
                        }
                        None => {
                            // Channel closed, shutdown gracefully
                            if lost_events_count > 0 {
                                error!("Lost {} events in the last second", lost_events_count);
                            }
                            break;
                        }
                    }
                }
                // Timer tick every second
                _ = interval.tick() => {
                    if lost_events_count > 0 {
                        error!("Lost {} events in the last second", lost_events_count);
                        lost_events_count = 0;
                    }
                }
            }
        }
    }

    /// Shutdown the error handler by closing the sender
    pub fn shutdown(&mut self) {
        if let Some(sender) = self.error_sender.take() {
            drop(sender);
        }
    }

    /// Handle timer migration detection events
    fn handle_timer_migration(&mut self, _ring_index: usize, data: &[u8]) {
        let event: &TimerMigrationMsg = match plain::from_bytes(data) {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to parse timer migration event: {:?}", e);
                return;
            }
        };

        // Timer migration detected - this is a critical error that invalidates measurements
        error!(
            r#"CRITICAL ERROR: Timer migration detected!
Expected CPU: {}, Actual CPU: {}
Timer pinning failed - measurements are no longer reliable.
This indicates either:
  1. Kernel version doesn't support BPF timer CPU pinning (requires 6.7+)
  2. Legacy fallback timer migration control failed
  This case should never happen, please report this as a bug with the distribution and kernel version.
Exiting to prevent incorrect performance measurements."#,
            event.expected_cpu, event.actual_cpu
        );

        std::process::exit(1);
    }

    /// Handle lost events
    fn handle_lost_events(&self, ring_index: usize, _data: &[u8]) {
        if let Some(sender) = &self.error_sender {
            let event = ErrorEvent::LostEvents;

            // If we're inside a Tokio runtime, avoid blocking the runtime thread.
            // Use an async send via spawn; otherwise, fall back to a blocking send.
            if tokio::runtime::Handle::try_current().is_ok() {
                let sender = sender.clone();
                tokio::spawn(async move {
                    if let Err(_) = sender.send(event).await {
                        // Channel is closed, fall back to direct logging
                        error!(
                            "Lost events notification on ring {} (error channel closed)",
                            ring_index
                        );
                    }
                });
            } else {
                // Not in a Tokio runtime — it's safe to block.
                if let Err(_) = sender.blocking_send(event) {
                    // Channel is closed, fall back to direct logging
                    error!(
                        "Lost events notification on ring {} (error channel closed)",
                        ring_index
                    );
                }
            }
        } else {
            // No sender available, fall back to direct logging
            error!(
                "Lost events notification on ring {} (no error sender)",
                ring_index
            );
        }
    }
}
