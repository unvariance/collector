use std::cell::RefCell;
use std::rc::Rc;

use log::error;
use tokio::sync::mpsc;

use bpf::{msg_type, BpfLoader, PerfMeasurementMsg};
use plain;

use crate::bpf_task_tracker::BpfTaskTracker;
use crate::bpf_timeslot_tracker::BpfTimeslotTracker;
use crate::metrics::Metric;
use crate::timeslot_data::TimeslotData;

/// Handles BPF performance measurements and composes them into timeslots
pub struct BpfPerfToTimeslot {
    current_timeslot: TimeslotData,
    // Channel for sending completed timeslots
    timeslot_tx: Option<mpsc::Sender<TimeslotData>>,
    // Error tracking for batched reporting
    error_counter: u64,
    last_error_report: std::time::Instant,
    // Task tracker for metadata lookup
    task_tracker: Rc<RefCell<BpfTaskTracker>>,
}

impl BpfPerfToTimeslot {
    /// Create a new BpfPerfToTimeslot processor
    pub fn new(
        bpf_loader: &mut BpfLoader,
        timeslot_tracker: Rc<RefCell<BpfTimeslotTracker>>,
        task_tracker: Rc<RefCell<BpfTaskTracker>>,
        timeslot_tx: mpsc::Sender<TimeslotData>,
    ) -> Rc<RefCell<Self>> {
        let processor = Rc::new(RefCell::new(Self {
            current_timeslot: TimeslotData::new(0), // Start with timestamp 0
            timeslot_tx: Some(timeslot_tx),
            error_counter: 0u64,
            last_error_report: std::time::Instant::now(),
            task_tracker,
        }));

        // Set up timeslot event subscription using subscribe_method
        timeslot_tracker
            .borrow_mut()
            .subscribe_method(processor.clone(), BpfPerfToTimeslot::on_new_timeslot);

        // Set up BPF event subscriptions
        {
            let dispatcher = bpf_loader.dispatcher_mut();

            dispatcher.subscribe_method(
                msg_type::MSG_TYPE_PERF_MEASUREMENT as u32,
                processor.clone(),
                BpfPerfToTimeslot::handle_perf_measurement,
            );
        }

        processor
    }

    /// Handle performance measurement events
    fn handle_perf_measurement(&mut self, _ring_index: usize, data: &[u8]) {
        let event: &PerfMeasurementMsg = match plain::from_bytes(data) {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to parse perf measurement event: {:?}", e);
                return;
            }
        };

        // Create metric from the performance measurements
        let metric = Metric::from_deltas(
            event.cycles_delta,
            event.instructions_delta,
            event.llc_misses_delta,
            event.cache_references_delta,
            event.time_delta_ns,
        );

        // Look up task metadata and update timeslot data
        let pid = event.pid;
        let metadata = self.task_tracker.borrow().lookup(pid).cloned();
        self.current_timeslot.update(pid, metadata, metric);
    }

    /// Handle new timeslot events
    fn on_new_timeslot(&mut self, _old_timeslot: u64, new_timeslot: u64) {
        // Create a new empty timeslot with the new timestamp
        let new_timeslot_data = TimeslotData::new(new_timeslot);

        // Take ownership of the current timeslot, replacing it with the new one
        let completed_timeslot = std::mem::replace(&mut self.current_timeslot, new_timeslot_data);

        // Try to send the completed timeslot to the writer
        if let Some(ref sender) = self.timeslot_tx {
            if let Err(_) = sender.try_send(completed_timeslot) {
                // Increment error count instead of printing immediately
                self.error_counter += 1;

                // Check if it's time to report errors (every 1 second)
                let now = std::time::Instant::now();
                if now.duration_since(self.last_error_report).as_secs() >= 1 {
                    // Report accumulated errors
                    if self.error_counter > 0 {
                        error!(
                            "Error sending timeslots to writer: {} errors in the last 1 second",
                            self.error_counter
                        );
                        self.error_counter = 0;
                    }
                    self.last_error_report = now;
                }
            }
        }
    }

    /// Shutdown the processor and close the timeslot channel
    pub fn shutdown(&mut self) {
        // Extract and drop the sender to close the channel
        if let Some(sender) = self.timeslot_tx.take() {
            drop(sender);
        }
    }
}
