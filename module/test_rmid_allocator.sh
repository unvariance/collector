#!/bin/bash
set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m' # No Color

# Function to print colored output
print_color() {
    color=$1
    shift
    echo -e "${color}$@${NC}"
}

# Clean up function
cleanup() {
    # Unload test module if loaded
    if lsmod | grep -q "rmid_allocator_test_module"; then
        sudo rmmod rmid_allocator_test_module
    fi
    # Clear dmesg buffer
    sudo dmesg -c > /dev/null
}

# Run cleanup on script exit
trap cleanup EXIT

echo "Building test module..."
make rmid_allocator_test

echo "Loading test module..."
sudo dmesg -c > /dev/null  # Clear dmesg buffer
sudo insmod build/rmid_allocator_test_module.ko

echo "Collecting test results..."
# Get all test results from dmesg and save to file
dmesg_output="/tmp/rmid_test_$$.txt"
sudo dmesg > "$dmesg_output"

# Extract just the test results to another file
test_results="/tmp/rmid_test_results_$$.txt"
grep "test_result:" "$dmesg_output" > "$test_results"

echo "Unloading test module..."
sudo rmmod rmid_allocator_test_module || true

# Parse test results
declare -i total_tests=0
declare -i passed_tests=0
declare -i failed_tests=0

echo -e "\nTest Results:"
echo "============="

# Read test results line by line
while IFS= read -r line; do
    # Extract test name and result using cut instead of regex
    test_name=$(echo "$line" | cut -d':' -f2)
    result=$(echo "$line" | cut -d':' -f3)
    
    if [ "$result" = "pass" ]; then
        print_color $GREEN "✓ $test_name"
        let passed_tests+=1
    else
        message=$(echo "$line" | cut -d':' -f4-)
        print_color $RED "✗ $test_name"
        [ ! -z "$message" ] && echo "  Error: $message"
        let failed_tests+=1
    fi
    let total_tests+=1
done < "$test_results"

# Clean up temporary files
rm -f "$dmesg_output" "$test_results"

echo -e "\nSummary:"
echo "========"
echo "Total tests: $total_tests"
print_color $GREEN "Passed: $passed_tests"
if [ $failed_tests -gt 0 ]; then
    print_color $RED "Failed: $failed_tests"
    echo -e "\nDmesg output:"
    echo "========"
    sudo dmesg
fi

# Exit with failure if any tests failed
[ $failed_tests -eq 0 ] || exit 1 