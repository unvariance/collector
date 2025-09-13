#!/bin/bash
set -e

# Function to log to stderr
log() {
    echo "$@" >&2
}

# Function to print usage
usage() {
    echo "Usage: $0 [-d <duration>] [-o <output_csv>] [-p <plot_file>]"
    echo
    echo "Run sync timer benchmark under various stress conditions"
    echo
    echo "Options:"
    echo "  -d <duration>   Duration for each test in seconds (default: 10)"
    echo "  -o <output_csv> Output CSV file (default: benchmark_results.csv)"
    echo "  -p <plot_file> Output plot file (default: benchmark_plot.pdf)"
    echo "  -h             Show this help message"
    exit 1
}

# Parse command line arguments
DURATION=10
OUTPUT_CSV="benchmark_results.csv"
PLOT_FILE="benchmark_plot.pdf"

while getopts "d:o:p:h" opt; do
    case $opt in
        d) DURATION="$OPTARG" ;;
        o) OUTPUT_CSV="$OPTARG" ;;
        p) PLOT_FILE="$OPTARG" ;;
        h) usage ;;
        \?) usage ;;
    esac
done

# Make sure we're in the module directory
cd "$(dirname "$0")"

# Create fresh results file
rm -f "$OUTPUT_CSV"

# Run baseline benchmark
log "Running baseline test..."
./benchmark_sync_timer.sh -n "baseline" -d "$DURATION" -o "$OUTPUT_CSV"

# CPU and scheduler stress
log "Running CPU stress test..."
./benchmark_sync_timer.sh -n "cpu_stress" \
    -s "stress-ng --cpu \$(nproc) --cpu-method matrixprod" \
    -d "$DURATION" -o "$OUTPUT_CSV"

# Memory and cache contention
log "Running memory stress test..."
./benchmark_sync_timer.sh -n "memory_stress" \
    -s "stress-ng --vm 4 --vm-bytes 75%" \
    -d "$DURATION" -o "$OUTPUT_CSV"

# Interrupt generation
log "Running interrupt stress test..."
./benchmark_sync_timer.sh -n "interrupt_stress" \
    -s "stress-ng --timer 8 --timer-freq 1000" \
    -d "$DURATION" -o "$OUTPUT_CSV"

# I/O and system call pressure
log "Running I/O stress test..."
./benchmark_sync_timer.sh -n "io_stress" \
    -s "stress-ng --hdd 4 --hdd-bytes 1G" \
    -d "$DURATION" -o "$OUTPUT_CSV"

log "Running syscall stress test..."
./benchmark_sync_timer.sh -n "syscall_stress" \
    -s "stress-ng --syscall 4" \
    -d "$DURATION" -o "$OUTPUT_CSV"

# Lock contention
log "Running lock stress test..."
./benchmark_sync_timer.sh -n "lock_stress" \
    -s "stress-ng --lockbus 4" \
    -d "$DURATION" -o "$OUTPUT_CSV"

# Mutex stress
# only run if `stress-ng --help` shows --mutex
if stress-ng --help | grep -q -- "--mutex"; then
    log "Running mutex stress test..."
    ./benchmark_sync_timer.sh -n "mutex_stress" \
        -s "stress-ng --mutex 8" \
        -d "$DURATION" -o "$OUTPUT_CSV"
fi

log "Benchmark complete. Results in $OUTPUT_CSV"
