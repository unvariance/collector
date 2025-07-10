/// Metrics structure to hold performance measurements collected from eBPF
#[derive(Debug, Default, Clone, Copy)]
pub struct Metric {
    /// Total CPU cycles
    pub cycles: u64,
    /// Total CPU instructions
    pub instructions: u64,
    /// Last-level cache misses
    pub llc_misses: u64,
    /// Cache references
    pub cache_references: u64,
    /// Total time measured in nanoseconds
    pub time_ns: u64,
}

impl Metric {
    /// Add another metric to this one, aggregating all values
    pub fn add(&mut self, other: &Metric) {
        self.cycles += other.cycles;
        self.instructions += other.instructions;
        self.llc_misses += other.llc_misses;
        self.cache_references += other.cache_references;
        self.time_ns += other.time_ns;
    }

    /// Create a metric from the raw performance counter deltas
    pub fn from_deltas(
        cycles: u64,
        instructions: u64,
        llc_misses: u64,
        cache_references: u64,
        time_ns: u64,
    ) -> Self {
        Self {
            cycles,
            instructions,
            llc_misses,
            cache_references,
            time_ns,
        }
    }
}
