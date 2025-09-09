package main

import (
	"github.com/prometheus/client_golang/prometheus"
)

func recordMetrics() {
	upMetric := prometheus.NewGauge(prometheus.GaugeOpts{
		Namespace: "perfpod",
		Subsystem: "memory_collector",
		Name:      "up_metric",
		Help:      "Test metric to confirm skeleton application functionality.",
	})
	prometheus.MustRegister(upMetric)

	upMetric.Set(1)
}
