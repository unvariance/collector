use crate::api::Event;
use protobuf::Enum;

/// EventMask corresponds to a set of enumerated Events.
///
/// Each bit in the mask represents a specific Event from the NRI API.
/// The bit position is (event_value - 1) since Event::UNKNOWN = 0 is never used.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EventMask(i32);

/// Returns the event mask of all valid events.
pub fn valid_events() -> EventMask {
    EventMask((1 << (Event::LAST.value() - 1)) - 1)
}

impl EventMask {
    /// Create a new empty EventMask.
    pub fn new() -> Self {
        Self(0)
    }

    /// Create a new EventMask with the given raw value.
    pub fn from_raw(value: i32) -> Self {
        Self(value)
    }

    /// Get the raw value of the EventMask.
    pub fn raw_value(&self) -> i32 {
        self.0
    }

    /// Set the given Events in the mask.
    pub fn set(&mut self, events: &[Event]) -> &mut Self {
        for &event in events {
            // Ensure the event is valid (not UNKNOWN and less than LAST)
            if event.value() != Event::UNKNOWN.value() && event.value() < Event::LAST.value() {
                self.0 |= 1 << (event.value() - 1);
            }
        }
        self
    }

    /// Clear the given Events in the mask.
    pub fn clear(&mut self, events: &[Event]) -> &mut Self {
        for &event in events {
            // Ensure the event is valid (not UNKNOWN and less than LAST)
            if event.value() != Event::UNKNOWN.value() && event.value() < Event::LAST.value() {
                self.0 &= !(1 << (event.value() - 1));
            }
        }
        self
    }

    /// Check if the given Event is set in the mask.
    pub fn is_set(&self, event: Event) -> bool {
        // Ensure the event is valid (not UNKNOWN and less than LAST)
        if event.value() == Event::UNKNOWN.value() || event.value() >= Event::LAST.value() {
            return false;
        }

        (self.0 & (1 << (event.value() - 1))) != 0
    }

    /// Return a human-readable string representation of the EventMask.
    pub fn pretty_string(&self) -> String {
        let mut events = Vec::new();
        let mut remaining_mask = self.0;

        // Check each bit from UNKNOWN+1 to LAST-1
        for event_value in 1..Event::LAST.value() {
            let event = match Event::from_i32(event_value) {
                Some(e) => e,
                None => continue,
            };

            if self.is_set(event) {
                events.push(format!("{:?}", event));
                // Clear the bit in our remaining mask
                remaining_mask &= !(1 << (event.value() - 1));
            }
        }

        // If there are any remaining bits set, add them as unknown
        if remaining_mask != 0 {
            events.push(format!("unknown(0x{:x})", remaining_mask));
        }

        events.join(",")
    }
}

impl From<i32> for EventMask {
    fn from(value: i32) -> Self {
        Self(value)
    }
}

impl From<EventMask> for i32 {
    fn from(mask: EventMask) -> Self {
        mask.0
    }
}

impl std::ops::BitOr for EventMask {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for EventMask {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl std::ops::BitAnd for EventMask {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl std::ops::BitAndAssign for EventMask {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_mask_set_clear_is_set() {
        let mut mask = EventMask::new();

        // Initially all bits should be unset
        assert_eq!(mask.raw_value(), 0);
        assert!(!mask.is_set(Event::CREATE_CONTAINER));

        // Set a single event
        mask.set(&[Event::CREATE_CONTAINER]);
        assert!(mask.is_set(Event::CREATE_CONTAINER));
        assert!(!mask.is_set(Event::STOP_CONTAINER));

        // Set multiple events
        mask.set(&[Event::STOP_CONTAINER, Event::START_CONTAINER]);
        assert!(mask.is_set(Event::CREATE_CONTAINER));
        assert!(mask.is_set(Event::STOP_CONTAINER));
        assert!(mask.is_set(Event::START_CONTAINER));

        // Clear an event
        mask.clear(&[Event::CREATE_CONTAINER]);
        assert!(!mask.is_set(Event::CREATE_CONTAINER));
        assert!(mask.is_set(Event::STOP_CONTAINER));
        assert!(mask.is_set(Event::START_CONTAINER));

        // Clear multiple events
        mask.clear(&[Event::STOP_CONTAINER, Event::START_CONTAINER]);
        assert!(!mask.is_set(Event::CREATE_CONTAINER));
        assert!(!mask.is_set(Event::STOP_CONTAINER));
        assert!(!mask.is_set(Event::START_CONTAINER));

        // Invalid events should be ignored
        mask.set(&[Event::UNKNOWN, Event::LAST]);
        assert_eq!(mask.raw_value(), 0);
    }

    #[test]
    fn test_event_mask_operators() {
        let mut mask1 = EventMask::new();
        mask1.set(&[Event::CREATE_CONTAINER]);

        let mut mask2 = EventMask::new();
        mask2.set(&[Event::STOP_CONTAINER]);

        // Test BitOr
        let combined = mask1 | mask2;
        assert!(combined.is_set(Event::CREATE_CONTAINER));
        assert!(combined.is_set(Event::STOP_CONTAINER));

        // Test BitOrAssign
        let mut mask3 = mask1;
        mask3 |= mask2;
        assert!(mask3.is_set(Event::CREATE_CONTAINER));
        assert!(mask3.is_set(Event::STOP_CONTAINER));

        // Test BitAnd
        let mut mask4 = EventMask::from_raw(-1); // All bits set in i32
        let intersection = mask4 & mask1;
        assert!(intersection.is_set(Event::CREATE_CONTAINER));
        assert!(!intersection.is_set(Event::STOP_CONTAINER));

        // Test BitAndAssign
        mask4 &= mask1;
        assert!(mask4.is_set(Event::CREATE_CONTAINER));
        assert!(!mask4.is_set(Event::STOP_CONTAINER));
    }

    #[test]
    fn test_pretty_string() {
        let mut mask = EventMask::new();
        mask.set(&[Event::CREATE_CONTAINER, Event::STOP_CONTAINER]);

        let pretty = mask.pretty_string();
        assert!(pretty.contains("CREATE_CONTAINER"));
        assert!(pretty.contains("STOP_CONTAINER"));
        assert!(!pretty.contains("START_CONTAINER"));
    }

    #[test]
    fn test_valid_events() {
        let valid = valid_events();

        // VALID_EVENTS should have all valid event bits set
        for event_value in 1..Event::LAST.value() {
            if let Some(event) = Event::from_i32(event_value) {
                assert!(
                    valid.is_set(event),
                    "Event {:?} should be set in valid_events()",
                    event
                );
            }
        }

        // But not UNKNOWN or LAST
        assert!(!valid.is_set(Event::UNKNOWN));
        assert!(!valid.is_set(Event::LAST));
    }
}
