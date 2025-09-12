use std::{cell::RefCell, collections::HashMap, rc::Rc};
use thiserror::Error;

use crate::{
    PerfRingError, Reader, ReaderError, SampleHeader, PERF_RECORD_LOST, PERF_RECORD_SAMPLE,
};

/// Errors that can occur during dispatch operations
#[derive(Error, Debug)]
pub enum DispatchError {
    #[error("reader error: {0}")]
    ReaderError(#[from] ReaderError),

    #[error("ring error: {0}")]
    RingError(#[from] PerfRingError),

    #[error("invalid message format: {0}")]
    InvalidFormat(String),
}

/// Tracks statistics for the dispatcher
#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    /// Number of sample events processed
    pub samples_processed: usize,

    /// Number of lost message events processed
    pub lost_events_processed: usize,

    /// Number of errors returned from callbacks
    pub callback_errors: usize,

    /// Number of messages with no registered callbacks
    pub dropped_messages: usize,
}

/// Dispatcher handles message distribution to subscribers based on message type
type Subscriber = Box<dyn FnMut(usize, &[u8])>;
type SubscribersMap = HashMap<u32, Vec<Subscriber>>;

pub struct Dispatcher {
    /// Callbacks for specific message types (message_type => vec of callbacks)
    sample_subscribers: SubscribersMap,

    /// Callbacks for lost sample events
    lost_subscribers: Vec<Subscriber>,

    /// Statistics counters
    stats: Stats,
}

impl Dispatcher {
    /// Creates a new dispatcher
    pub fn new() -> Self {
        Dispatcher {
            sample_subscribers: HashMap::new(),
            lost_subscribers: Vec::new(),
            stats: Stats::default(),
        }
    }

    /// Returns the current statistics
    pub fn stats(&self) -> Stats {
        self.stats
    }

    /// Subscribe to events of a specific message type
    pub fn subscribe<F>(&mut self, message_type: u32, callback: F)
    where
        F: FnMut(usize, &[u8]) + 'static,
    {
        self.sample_subscribers
            .entry(message_type)
            .or_default()
            .push(Box::new(callback));
    }

    /// Subscribe to lost sample events
    pub fn subscribe_lost_samples<F>(&mut self, callback: F)
    where
        F: FnMut(usize, &[u8]) + 'static,
    {
        self.lost_subscribers.push(Box::new(callback));
    }

    /// Subscribe to events of a specific message type with a method from a struct
    pub fn subscribe_method<T: 'static>(
        &mut self,
        message_type: u32,
        instance: Rc<RefCell<T>>,
        method: fn(&mut T, usize, &[u8]),
    ) {
        let callback = move |ring_index, data: &[u8]| {
            method(&mut instance.borrow_mut(), ring_index, data);
        };
        self.subscribe(message_type, callback);
    }

    /// Dispatch events from the reader to registered subscribers
    pub fn dispatch(&mut self, reader: &mut Reader) -> Result<(), DispatchError> {
        if reader.is_empty() {
            return Ok(());
        }

        // Get the current ring and its index
        let (ring, ring_index) = reader.current_ring()?;

        let size = ring.peek_size()?;
        let mut event_data = vec![0u8; size];
        ring.peek_copy(&mut event_data, 0)?;

        // Check the event type
        match ring.peek_type() {
            PERF_RECORD_SAMPLE => {
                // The message format after the perf header is defined by the SampleHeader struct

                let header: &SampleHeader = plain::from_bytes(&event_data).map_err(|_e| {
                    DispatchError::InvalidFormat(
                        "Sample event too small to contain message type and timestamp".to_string(),
                    )
                })?;

                // Check if we have subscribers for this message type
                if let Some(subscribers) = self.sample_subscribers.get_mut(&header.type_) {
                    // Call each subscriber with the ring index and message data
                    for subscriber in subscribers {
                        subscriber(ring_index, &event_data);
                    }
                    self.stats.samples_processed += 1;
                } else {
                    // No subscribers for this message type
                    self.stats.dropped_messages += 1;
                }
            }
            PERF_RECORD_LOST => {
                // For lost events, we just pass the raw event data

                // Call lost sample subscribers
                for subscriber in &mut self.lost_subscribers {
                    subscriber(ring_index, &event_data);
                }
                self.stats.lost_events_processed += 1;
            }
            _ => {
                // Unhandled event type, just track as dropped
                self.stats.dropped_messages += 1;
            }
        }

        // Pop the event from the reader
        reader.pop()?;

        Ok(())
    }

    /// Dispatches all available events until the reader is empty
    pub fn dispatch_all(&mut self, reader: &mut Reader) -> Result<(), DispatchError> {
        while !reader.is_empty() {
            self.dispatch(reader)?;
        }
        Ok(())
    }

    /// Dispatches events until the reader is empty or the next event timestamp is >= until_timestamp_ns
    pub fn dispatch_until(
        &mut self,
        reader: &mut Reader,
        until_timestamp_ns: u64,
    ) -> Result<(), DispatchError> {
        while !reader.is_empty() {
            // Check if the next event's timestamp is past our cutoff
            let timestamp = reader.peek_timestamp()?;
            if timestamp >= until_timestamp_ns {
                return Ok(());
            }
            self.dispatch(reader)?;
        }
        Ok(())
    }

    /// Dispatches events conservatively while all rings have events available.
    /// This ensures no event reordering by only processing events when we know
    /// no earlier timestamp can appear from any ring (since each ring outputs
    /// events in monotonically increasing timestamp order).
    pub fn dispatch_conservative(&mut self, reader: &mut Reader) -> Result<(), DispatchError> {
        while reader.all_rings_non_empty() {
            self.dispatch(reader)?;
        }
        Ok(())
    }
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use plain::Plain;

    use super::*;
    use crate::PerfRing;
    use std::cell::RefCell;
    use std::rc::Rc;

    // Constants for test message types
    const MSG_TYPE_FOO: u32 = 1;
    const MSG_TYPE_BAR: u32 = 2;

    #[repr(C)]
    struct TestMessage {
        header: SampleHeader,
        data: [u8; 8],
    }
    unsafe impl Plain for TestMessage {}

    // Create a test message
    fn create_test_message(msg_type: u32, timestamp: u64, data: &[u8]) -> Vec<u8> {
        let mut message = Vec::with_capacity(size_of::<TestMessage>());
        let msg = TestMessage {
            header: SampleHeader {
                size: 8,
                type_: msg_type,
                timestamp,
            },
            data: data.try_into().unwrap(),
        };
        message.extend_from_slice(unsafe { plain::as_bytes(&msg)[4..].as_ref() });
        message
    }

    #[test]
    fn test_dispatcher_basic() {
        // Setup test rings and reader
        let page_size = 4096u64;
        let n_pages = 2u32;
        let mut data1 = vec![0u8; (page_size * (1 + u64::from(n_pages))) as usize];
        let mut data2 = vec![0u8; (page_size * (1 + u64::from(n_pages))) as usize];

        let mut ring1 =
            unsafe { PerfRing::init_contiguous(&mut data1, n_pages, page_size).unwrap() };
        let mut ring2 =
            unsafe { PerfRing::init_contiguous(&mut data2, n_pages, page_size).unwrap() };

        // Create the reader
        let mut reader = Reader::new();
        reader
            .add_ring(unsafe { PerfRing::init_contiguous(&mut data1, n_pages, page_size).unwrap() })
            .unwrap();
        reader
            .add_ring(unsafe { PerfRing::init_contiguous(&mut data2, n_pages, page_size).unwrap() })
            .unwrap();

        // Create the dispatcher
        let mut dispatcher = Dispatcher::new();

        // Setup message counters
        let foo_counter = Rc::new(RefCell::new(0));
        let bar_counter = Rc::new(RefCell::new(0));
        let lost_counter = Rc::new(RefCell::new(0));

        // Subscribe to message types
        {
            let foo_counter = foo_counter.clone();
            dispatcher.subscribe(MSG_TYPE_FOO, move |_, data| {
                *foo_counter.borrow_mut() += 1;
                assert_eq!(data.len(), size_of::<TestMessage>());
                let msg: &TestMessage = plain::from_bytes(data).unwrap();
                assert_eq!(&msg.data, b"FOO DATA");
            });
        }

        {
            let bar_counter = bar_counter.clone();
            dispatcher.subscribe(MSG_TYPE_BAR, move |_, data| {
                *bar_counter.borrow_mut() += 1;
                assert_eq!(data.len(), size_of::<TestMessage>());
                let msg: &TestMessage = plain::from_bytes(data).unwrap();
                assert_eq!(&msg.data, b"BAR DATA");
            });
        }

        {
            let lost_counter = lost_counter.clone();
            dispatcher.subscribe_lost_samples(move |_, _| {
                *lost_counter.borrow_mut() += 1;
            });
        }

        // Write test messages
        ring1.start_write_batch();

        // FOO message
        let foo_msg = create_test_message(MSG_TYPE_FOO, 100, b"FOO DATA");
        ring1.write(&foo_msg, PERF_RECORD_SAMPLE).unwrap();

        // BAR message
        let bar_msg = create_test_message(MSG_TYPE_BAR, 200, b"BAR DATA");
        ring1.write(&bar_msg, PERF_RECORD_SAMPLE).unwrap();

        // Lost event
        let lost_data = [0u8; 8];
        ring1.write(&lost_data, PERF_RECORD_LOST).unwrap();

        ring1.finish_write_batch();

        // Write another message to ring2
        ring2.start_write_batch();
        let foo_msg2 = create_test_message(MSG_TYPE_FOO, 150, b"FOO DATA");
        ring2.write(&foo_msg2, PERF_RECORD_SAMPLE).unwrap();
        ring2.finish_write_batch();

        // Start reading
        reader.start().unwrap();

        // Dispatch all events
        dispatcher.dispatch_all(&mut reader).unwrap();

        // Check counters
        assert_eq!(*foo_counter.borrow(), 2);
        assert_eq!(*bar_counter.borrow(), 1);
        assert_eq!(*lost_counter.borrow(), 1);

        // Check statistics
        let stats = dispatcher.stats();
        assert_eq!(stats.samples_processed, 3);
        assert_eq!(stats.lost_events_processed, 1);
        assert_eq!(stats.callback_errors, 0);
        assert_eq!(stats.dropped_messages, 0);

        // Finish reading
        reader.finish().unwrap();
    }

    #[test]
    fn test_dispatcher_no_subscribers() {
        // Setup test rings and reader
        let page_size = 4096u64;
        let n_pages = 2u32;
        let mut data = vec![0u8; (page_size * (1 + u64::from(n_pages))) as usize];

        let mut ring = unsafe { PerfRing::init_contiguous(&mut data, n_pages, page_size).unwrap() };

        // Create the reader
        let mut reader = Reader::new();
        reader
            .add_ring(unsafe { PerfRing::init_contiguous(&mut data, n_pages, page_size).unwrap() })
            .unwrap();

        // Create the dispatcher
        let mut dispatcher = Dispatcher::new();

        // Write a message with no subscribers
        ring.start_write_batch();
        let unknown_msg = create_test_message(999, 100, b"UNKNOWN ");
        ring.write(&unknown_msg, PERF_RECORD_SAMPLE).unwrap();
        ring.finish_write_batch();

        // Start reading
        reader.start().unwrap();

        // Dispatch
        dispatcher.dispatch_all(&mut reader).unwrap();

        // Check statistics
        let stats = dispatcher.stats();
        assert_eq!(stats.dropped_messages, 1);
        assert_eq!(stats.samples_processed, 0);

        // Finish reading
        reader.finish().unwrap();
    }

    #[test]
    fn test_dispatcher_using_instance_methods() {
        // Setup test rings and reader
        let page_size = 4096u64;
        let n_pages = 2u32;
        let mut data = vec![0u8; (page_size * (1 + u64::from(n_pages))) as usize];

        let mut ring = unsafe { PerfRing::init_contiguous(&mut data, n_pages, page_size).unwrap() };

        // Create the reader
        let mut reader = Reader::new();
        reader
            .add_ring(unsafe { PerfRing::init_contiguous(&mut data, n_pages, page_size).unwrap() })
            .unwrap();

        // Handler with instance methods
        struct MyHandler {
            foo_counter: usize,
            bar_counter: usize,
        }

        impl MyHandler {
            fn handle_foo(&mut self, _ring_index: usize, data: &[u8]) {
                self.foo_counter += 1;
                let msg: &TestMessage = plain::from_bytes(data).unwrap();
                assert_eq!(&msg.data, b"FOO DATA");
            }

            fn handle_bar(&mut self, _ring_index: usize, data: &[u8]) {
                self.bar_counter += 1;
                let msg: &TestMessage = plain::from_bytes(data).unwrap();
                assert_eq!(&msg.data, b"BAR DATA");
            }
        }

        // Create handler
        let handler = Rc::new(RefCell::new(MyHandler {
            foo_counter: 0,
            bar_counter: 0,
        }));

        // Create the dispatcher
        let mut dispatcher = Dispatcher::new();

        // Register instance methods as callbacks
        {
            let handler_clone = handler.clone();
            dispatcher.subscribe(MSG_TYPE_FOO, move |idx, data| {
                handler_clone.borrow_mut().handle_foo(idx, data);
            });
        }

        {
            let handler_clone = handler.clone();
            dispatcher.subscribe(MSG_TYPE_BAR, move |idx, data| {
                handler_clone.borrow_mut().handle_bar(idx, data);
            });
        }

        // Write test messages
        ring.start_write_batch();
        let foo_msg = create_test_message(MSG_TYPE_FOO, 100, b"FOO DATA");
        ring.write(&foo_msg, PERF_RECORD_SAMPLE).unwrap();

        let bar_msg = create_test_message(MSG_TYPE_BAR, 200, b"BAR DATA");
        ring.write(&bar_msg, PERF_RECORD_SAMPLE).unwrap();
        ring.finish_write_batch();

        // Start reading
        reader.start().unwrap();

        // Dispatch all events
        dispatcher.dispatch_all(&mut reader).unwrap();

        // Check handler counters
        let handler_ref = handler.borrow();
        assert_eq!(handler_ref.foo_counter, 1);
        assert_eq!(handler_ref.bar_counter, 1);

        // Finish reading
        reader.finish().unwrap();
    }

    #[test]
    fn test_invalid_message_format() {
        // Setup test rings and reader
        let page_size = 4096u64;
        let n_pages = 2u32;
        let mut data = vec![0u8; (page_size * (1 + u64::from(n_pages))) as usize];

        let mut ring = unsafe { PerfRing::init_contiguous(&mut data, n_pages, page_size).unwrap() };

        // Create the reader
        let mut reader = Reader::new();
        reader
            .add_ring(unsafe { PerfRing::init_contiguous(&mut data, n_pages, page_size).unwrap() })
            .unwrap();

        // Create the dispatcher
        let mut dispatcher = Dispatcher::new();

        // Subscribe to ensure we're testing the message format check
        dispatcher.subscribe(MSG_TYPE_FOO, |_, _| {});

        // Write an incomplete message (missing timestamp)
        ring.start_write_batch();
        // Only write the message type, not the timestamp
        let incomplete_msg = vec![1, 0, 0, 0]; // message type 1 in little-endian
        ring.write(&incomplete_msg, PERF_RECORD_SAMPLE).unwrap();
        ring.finish_write_batch();

        // Start reading
        reader.start().unwrap();

        // Dispatch should fail with InvalidFormat
        let result = dispatcher.dispatch(&mut reader);
        assert!(result.is_err());
        match result {
            Err(DispatchError::InvalidFormat(_)) => {}
            _ => panic!("Expected InvalidFormat error"),
        }

        // Finish reading
        reader.finish().unwrap();
    }
}
