#!/bin/bash
set -e

# Generate random temporary filenames
RUN_ID=$(openssl rand -hex 8)
TRACE_DATA="/tmp/trace_data_$RUN_ID"
TRACE_OUTPUT="/tmp/trace_output_$RUN_ID.txt"

echo "Loading kernel module..."
sudo insmod build/collector.ko

echo "Verifying module is loaded..."
lsmod | grep collector || {
    echo "Error: Module failed to load"
    exit 1
}

echo "Setting up tracing..."
sudo trace-cmd start -e measure_perf_counters -e rdt_sample

echo "Collecting samples for 1 second..."
sleep 1

echo "Stopping trace..."
sudo trace-cmd stop

echo "Extracting trace data to $TRACE_DATA..."
sudo trace-cmd extract -o "$TRACE_DATA"

echo "Reading trace report..."
sudo trace-cmd report -i "$TRACE_DATA" > "$TRACE_OUTPUT"

echo "Head of trace report:"
head "$TRACE_OUTPUT"

echo "Tail of trace report:"
tail "$TRACE_OUTPUT"

echo "Head of is_context_switch=0:"
grep "is_context_switch=0" "$TRACE_OUTPUT" | head -n 300

echo "Head of is_context_switch=1:"
grep "is_context_switch=1" "$TRACE_OUTPUT" | head

echo "Head of resctrl:"
grep "rdt_sample:" "$TRACE_OUTPUT" | head -n 20

echo "Validating output..."
# Check if we have any trace entries
SAMPLE_COUNT=$(grep "measure_perf_counters:" "$TRACE_OUTPUT" | wc -l)
CPU_COUNT=$(nproc)
EXPECTED_MIN=$((900 * CPU_COUNT))

if [ $SAMPLE_COUNT -lt $EXPECTED_MIN ]; then
    echo "Error: Got $SAMPLE_COUNT trace entries, expected at least $EXPECTED_MIN"
    exit 1
fi

echo "Unloading module..."
sudo rmmod collector

# Verify module unloaded successfully
echo "Verifying unload..."
! lsmod | grep collector

echo "Cleaning up trace..."
sudo trace-cmd reset

echo "Test completed successfully!"
echo "Sample count: $SAMPLE_COUNT"
echo "Trace data: $TRACE_DATA"
echo "Trace output: $TRACE_OUTPUT" 