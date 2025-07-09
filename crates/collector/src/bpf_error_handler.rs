use std::cell::RefCell;
use std::rc::Rc;

use log::error;

use bpf::{msg_type, BpfLoader, TimerMigrationMsg};

/// BPF Error Handler manages error-related BPF events like timer migration and lost samples
pub struct BpfErrorHandler {
    // Currently no internal state needed, but struct is kept for future extensibility
}

impl BpfErrorHandler {
    /// Create a new BpfErrorHandler and subscribe to error events
    pub fn new(bpf_loader: &mut BpfLoader) -> Rc<RefCell<Self>> {
        let handler = Rc::new(RefCell::new(Self {}));

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
        error!("Lost events notification on ring {}", ring_index);
    }
}
