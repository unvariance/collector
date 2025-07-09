use std::cell::RefCell;
use std::rc::Rc;

use log::error;
use timeslot::MinTracker;

use bpf::{msg_type, BpfLoader, TimerFinishedProcessingMsg};

/// Callback type for new timeslot events
/// Receives (old_timeslot, new_timeslot) where timeslot is the timestamp
type NewTimeslotCallback = Box<dyn Fn(u64, u64)>;

/// BPF Timeslot Tracker manages timer events and notifies subscribers when timeslots change
pub struct BpfTimeslotTracker {
    min_tracker: MinTracker,
    last_min_slot: Option<u64>,
    subscribers: Vec<NewTimeslotCallback>,
}

impl BpfTimeslotTracker {
    /// Create a new BpfTimeslotTracker and subscribe to timer events
    pub fn new(bpf_loader: &mut BpfLoader, num_cpus: usize) -> Rc<RefCell<Self>> {
        let tracker = Rc::new(RefCell::new(Self {
            min_tracker: MinTracker::new(1_000_000, num_cpus),
            last_min_slot: None,
            subscribers: Vec::new(),
        }));

        // Subscribe to timer finished processing events
        let dispatcher = bpf_loader.dispatcher_mut();
        dispatcher.subscribe_method(
            msg_type::MSG_TYPE_TIMER_FINISHED_PROCESSING as u32,
            tracker.clone(),
            BpfTimeslotTracker::handle_timer_finished_processing,
        );

        tracker
    }

    /// Subscribe to new timeslot events
    /// Callback receives (old_timeslot, new_timeslot) timestamps
    pub fn subscribe(&mut self, callback: impl Fn(u64, u64) + 'static) {
        self.subscribers.push(Box::new(callback));
    }

    /// Handle timer finished processing events
    fn handle_timer_finished_processing(&mut self, ring_index: usize, data: &[u8]) {
        let event: &TimerFinishedProcessingMsg = match plain::from_bytes(data) {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to parse timer finished processing event: {:?}", e);
                return;
            }
        };

        // Update the min tracker with the CPU ID and timestamp
        let timestamp = event.header.timestamp;

        if let Err(e) = self.min_tracker.update(ring_index, timestamp) {
            error!("Failed to update min tracker: {:?}", e);
            return;
        }

        // Check if the minimum time slot has changed
        let new_min_slot = self.min_tracker.get_min();
        if new_min_slot != self.last_min_slot {
            let old_timeslot = self.last_min_slot.unwrap_or(0);
            let new_timeslot = new_min_slot.unwrap_or(0);

            // Update the last min slot
            self.last_min_slot = new_min_slot;

            // Only notify subscribers if we have a valid transition
            if self.last_min_slot.is_some() || old_timeslot > 0 {
                // Notify all subscribers of the timeslot change
                for callback in &self.subscribers {
                    callback(old_timeslot, new_timeslot);
                }
            }
        }
    }
}
