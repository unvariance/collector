use std::cell::RefCell;
use std::rc::Rc;

use log::error;

use crate::task_metadata::{TaskCollection, TaskMetadata};
use bpf::{msg_type, BpfLoader, TaskFreeMsg, TaskMetadataMsg};

/// BPF Task Tracker manages task metadata and task free events
pub struct BpfTaskTracker {
    task_collection: TaskCollection,
}

impl BpfTaskTracker {
    /// Create a new BpfTaskTracker and subscribe to task events
    pub fn new(bpf_loader: &mut BpfLoader) -> Rc<RefCell<Self>> {
        let tracker = Rc::new(RefCell::new(Self {
            task_collection: TaskCollection::new(),
        }));

        // Subscribe to task events
        let dispatcher = bpf_loader.dispatcher_mut();

        // Subscribe to task metadata events
        let tracker_clone = tracker.clone();
        dispatcher.subscribe(
            msg_type::MSG_TYPE_TASK_METADATA as u32,
            move |ring_index, data| {
                tracker_clone
                    .borrow_mut()
                    .handle_task_metadata(ring_index, data);
            },
        );

        // Subscribe to task free events
        let tracker_clone = tracker.clone();
        dispatcher.subscribe(
            msg_type::MSG_TYPE_TASK_FREE as u32,
            move |ring_index, data| {
                tracker_clone
                    .borrow_mut()
                    .handle_task_free(ring_index, data);
            },
        );

        tracker
    }

    /// Look up task metadata by PID
    pub fn lookup(&self, pid: u32) -> Option<&TaskMetadata> {
        self.task_collection.lookup(pid)
    }

    /// Flush queued task removals
    pub fn flush_removals(&mut self) {
        self.task_collection.flush_removals();
    }

    /// Handle task metadata events
    fn handle_task_metadata(&mut self, _ring_index: usize, data: &[u8]) {
        let event: &TaskMetadataMsg = match plain::from_bytes(data) {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to parse task metadata event: {:?}", e);
                return;
            }
        };

        // Create task metadata and add to collection
        let metadata = TaskMetadata::new(event.pid, event.comm, event.cgroup_id);
        self.task_collection.add(metadata);
    }

    /// Handle task free events
    fn handle_task_free(&mut self, _ring_index: usize, data: &[u8]) {
        let event: &TaskFreeMsg = match plain::from_bytes(data) {
            Ok(event) => event,
            Err(e) => {
                error!("Failed to parse task free event: {:?}", e);
                return;
            }
        };

        // Queue the task for removal
        self.task_collection.queue_removal(event.pid);
    }
}
