use std::cell::RefCell;
use std::rc::Rc;

use arrow_array::RecordBatch;
use tokio::sync::mpsc;

use bpf::BpfLoader;

use crate::bpf_error_handler::{BpfErrorHandler, ErrorEvent};
use crate::bpf_perf_to_timeslot::BpfPerfToTimeslot;
use crate::bpf_perf_to_trace::BpfPerfToTrace;
use crate::bpf_task_tracker::BpfTaskTracker;
use crate::bpf_timeslot_tracker::BpfTimeslotTracker;
use crate::timeslot_data::TimeslotData;

/// Enum for selecting processor mode and channel type
pub enum ProcessorMode {
    Timeslot(mpsc::Sender<TimeslotData>),
    Trace(mpsc::Sender<RecordBatch>),
}

// Application coordinator for BPF components with dual mode support
pub struct PerfEventProcessor {
    // BPF timeslot tracker
    _timeslot_tracker: Rc<RefCell<BpfTimeslotTracker>>,
    // BPF error handler
    error_handler: Rc<RefCell<BpfErrorHandler>>,
    // BPF task tracker
    _task_tracker: Rc<RefCell<BpfTaskTracker>>,
    // Processors (exactly one will be Some based on mode)
    _perf_to_timeslot: Option<Rc<RefCell<BpfPerfToTimeslot>>>,
    _perf_to_trace: Option<Rc<RefCell<BpfPerfToTrace>>>,
}

impl PerfEventProcessor {
    // Create a new PerfEventProcessor with mode-specific configuration
    pub fn new(
        bpf_loader: &mut BpfLoader,
        num_cpus: usize,
        mode: ProcessorMode,
    ) -> Rc<RefCell<Self>> {
        // Create BpfTimeslotTracker (always present)
        let timeslot_tracker = BpfTimeslotTracker::new(bpf_loader, num_cpus);

        // Create BpfErrorHandler
        let error_handler = BpfErrorHandler::new(bpf_loader);

        // Create BpfTaskTracker with timeslot tracker reference
        let task_tracker = BpfTaskTracker::new(bpf_loader, timeslot_tracker.clone());

        // Create mode-specific processor
        let (perf_to_timeslot, perf_to_trace) = match mode {
            ProcessorMode::Timeslot(timeslot_tx) => {
                // Create timeslot composition processor
                let perf_to_timeslot = BpfPerfToTimeslot::new(
                    bpf_loader,
                    timeslot_tracker.clone(),
                    task_tracker.clone(),
                    timeslot_tx,
                );
                (Some(perf_to_timeslot), None)
            }
            ProcessorMode::Trace(batch_tx) => {
                // Create trace processor with default capacity of 1000 rows
                let perf_to_trace = BpfPerfToTrace::new(
                    bpf_loader,
                    timeslot_tracker.clone(),
                    task_tracker.clone(),
                    batch_tx,
                    32 * 1024, // Default batch capacity
                );
                (None, Some(perf_to_trace))
            }
        };

        Rc::new(RefCell::new(Self {
            _timeslot_tracker: timeslot_tracker,
            error_handler,
            _task_tracker: task_tracker,
            _perf_to_timeslot: perf_to_timeslot,
            _perf_to_trace: perf_to_trace,
        }))
    }

    /// Take the receiver from the error handler for running the error reporting task
    pub fn take_error_receiver(&mut self) -> Option<mpsc::Receiver<ErrorEvent>> {
        self.error_handler.borrow_mut().take_receiver()
    }

    /// Run the error reporting task
    pub async fn run_error_reporting(receiver: mpsc::Receiver<ErrorEvent>) {
        BpfErrorHandler::run_error_reporting(receiver).await;
    }

    // Shutdown the processor and close all channels
    pub fn shutdown(&mut self) {
        // Shutdown the error handler first
        self.error_handler.borrow_mut().shutdown();

        // Shutdown the active processor based on mode
        if let Some(ref timeslot_proc) = self._perf_to_timeslot {
            timeslot_proc.borrow_mut().shutdown();
        }
        if let Some(ref trace_proc) = self._perf_to_trace {
            trace_proc.borrow_mut().shutdown();
        }
    }
}
