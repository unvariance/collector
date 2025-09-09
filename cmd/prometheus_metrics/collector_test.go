package main

import (
	"fmt"
	"strings"
	"testing"
	"time"

	"github.com/prometheus/client_golang/prometheus/testutil"
	"github.com/stretchr/testify/require"
)

func TestMetricsUp(t *testing.T) {

	// Initialize module/server for standalone integration test
	go main()
	// time.Sleep(1 * time.Second)

	// Set values for standalone integration test
	serverURL := "http://localhost:2112"
	metricName := "perfpod_memory_collector_up_metric"
	metricHelpText := "Test metric to confirm skeleton application functionality."
	metricType := "gauge"
	expectedMetricValue := 1

	// Check for test result every 100 milliseconds with a 1 second limit
	require.Eventuallyf(t, func() bool {

		// Exact multiline metric template required by testutil.ScrapeAndCompare:

		// # HELP perfpod_memory_collector_up_metric Test metric to confirm skeleton application functionality.
		// # TYPE perfpod_memory_collector_up_metric gauge
		// perfpod_memory_collector_up_metric 1

		// Generalized metric template
		expected := fmt.Sprintf("\n# HELP %s %s\n# TYPE %s %s\n%s %d\n", metricName, metricHelpText, metricName, metricType, metricName, expectedMetricValue)

		// Check current server metrics against expected string/value
		if err := testutil.ScrapeAndCompare(serverURL+"/metrics", strings.NewReader(expected), metricName); err == nil {
			return true

			// Returns an error and error message if the expected string/value is not found
		} else {
			t.Log(err.Error())
			return false
		}
	}, time.Second, 100*time.Millisecond, "Could not find metric %s with value %d", metricName, expectedMetricValue)
}
