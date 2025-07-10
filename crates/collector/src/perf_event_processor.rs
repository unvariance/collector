use std::cell::RefCell;
use std::rc::Rc;

use tokio::sync::mpsc;

use bpf::BpfLoader;

use crate::bpf_error_handler::BpfErrorHandler;
use crate::bpf_perf_to_timeslot::BpfPerfToTimeslot;
use crate::bpf_task_tracker::BpfTaskTracker;
use crate::bpf_timeslot_tracker::BpfTimeslotTracker;
use crate::timeslot_data::TimeslotData;

// Simplified application coordinator for BPF components
pub struct PerfEventProcessor {
    // BPF timeslot tracker
    _timeslot_tracker: Rc<RefCell<BpfTimeslotTracker>>,
    // BPF error handler
    _error_handler: Rc<RefCell<BpfErrorHandler>>,
    // BPF task tracker
    _task_tracker: Rc<RefCell<BpfTaskTracker>>,
    // Timeslot composition processor
    _perf_to_timeslot: Rc<RefCell<BpfPerfToTimeslot>>,
}

impl PerfEventProcessor {
    // Create a new PerfEventProcessor with a timeslot sender
    pub fn new(
        bpf_loader: &mut BpfLoader,
        num_cpus: usize,
        timeslot_tx: mpsc::Sender<TimeslotData>,
    ) -> Rc<RefCell<Self>> {
        // Create BpfTimeslotTracker
        let timeslot_tracker = BpfTimeslotTracker::new(bpf_loader, num_cpus);

        // Create BpfErrorHandler
        let error_handler = BpfErrorHandler::new(bpf_loader);

        // Create BpfTaskTracker with timeslot tracker reference
        let task_tracker = BpfTaskTracker::new(bpf_loader, timeslot_tracker.clone());

        // Create timeslot composition processor
        let perf_to_timeslot = BpfPerfToTimeslot::new(
            bpf_loader,
            timeslot_tracker.clone(),
            task_tracker.clone(),
            timeslot_tx,
        );

        let processor = Rc::new(RefCell::new(Self {
            _timeslot_tracker: timeslot_tracker,
            _error_handler: error_handler,
            _task_tracker: task_tracker,
            _perf_to_timeslot: perf_to_timeslot,
        }));

        processor
    }

    // Shutdown the processor and close all channels
    pub fn shutdown(&mut self) {
        // Shutdown the timeslot composition processor
        self._perf_to_timeslot.borrow_mut().shutdown();
    }
}
