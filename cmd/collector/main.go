package main

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"os/signal"
	"time"
	"unsafe"

	"github.com/cilium/ebpf/link"
	"github.com/cilium/ebpf/rlimit"
	"github.com/unvariance/collector/pkg/aggregate"
	ourperf "github.com/unvariance/collector/pkg/perf"
	"github.com/unvariance/collector/pkg/perf_ebpf"
	"github.com/xitongsys/parquet-go-source/local"
	"github.com/xitongsys/parquet-go/parquet"
	"github.com/xitongsys/parquet-go/source"
	"github.com/xitongsys/parquet-go/writer"
	"golang.org/x/sys/unix"
)

// MetricsRecord represents a single row in our parquet file
type MetricsRecord struct {
	StartTime    int64 `parquet:"name=start_time, type=INT64"`
	EndTime      int64 `parquet:"name=end_time, type=INT64"`
	RMID         int32 `parquet:"name=rmid, type=INT32"`
	Cycles       int64 `parquet:"name=cycles, type=INT64"`
	Instructions int64 `parquet:"name=instructions, type=INT64"`
	LLCMisses    int64 `parquet:"name=llc_misses, type=INT64"`
	Duration     int64 `parquet:"name=duration, type=INT64"`
}

// parquetWriter wraps parquet file writing functionality
type parquetWriter struct {
	file   source.ParquetFile
	writer *writer.ParquetWriter
}

// newParquetWriter creates a new parquet writer with the given filename
func newParquetWriter(filename string) (*parquetWriter, error) {
	file, err := local.NewLocalFileWriter(filename)
	if err != nil {
		return nil, fmt.Errorf("failed to create parquet file: %w", err)
	}

	// Create parquet writer with 8MB row group size and Snappy compression
	pw, err := writer.NewParquetWriter(file, new(MetricsRecord), 8*1024*1024)
	if err != nil {
		file.Close()
		return nil, fmt.Errorf("failed to create parquet writer: %w", err)
	}

	// Set Snappy compression
	pw.CompressionType = parquet.CompressionCodec_SNAPPY

	return &parquetWriter{
		file:   file,
		writer: pw,
	}, nil
}

// writeTimeSlots writes the completed time slots to the parquet file
func (pw *parquetWriter) writeTimeSlots(slots []*aggregate.TimeSlot) error {
	for _, slot := range slots {
		for rmid, agg := range slot.Aggregations {
			record := &MetricsRecord{
				StartTime:    int64(slot.StartTime),
				EndTime:      int64(slot.EndTime),
				RMID:         int32(rmid),
				Cycles:       int64(agg.Cycles),
				Instructions: int64(agg.Instructions),
				LLCMisses:    int64(agg.LLCMisses),
				Duration:     int64(agg.Duration),
			}
			if err := pw.writer.Write(record); err != nil {
				return fmt.Errorf("failed to write record: %w", err)
			}
		}
	}
	return nil
}

// close properly closes the parquet writer and underlying file
func (pw *parquetWriter) close() error {
	if err := pw.writer.WriteStop(); err != nil {
		pw.file.Close()
		return fmt.Errorf("failed to stop parquet writer: %w", err)
	}
	return pw.file.Close()
}

// Note: taskCounterEvent is auto-generated by bpf2go
// Note: taskCounterRmidMetadata is auto-generated by bpf2go

// nanotime returns monotonic time in nanoseconds. We get this from the runtime
//
//go:linkname nanotime runtime.nanotime
func nanotime() int64

// dumpRmidMap dumps all valid RMIDs and their metadata
func dumpRmidMap(objs *taskCounterObjects) {
	var key uint32
	var metadata taskCounterRmidMetadata

	log.Println("Dumping RMID map contents:")
	log.Println("Index\tRMID\tComm\tTgid\tTimestamp\tValid")
	log.Println("-----\t----\t----\t----\t---------\t-----")

	for i := uint32(0); i < 512; i++ { // max_entries is 512 from task_counter.c
		key = i
		err := objs.RmidMap.Lookup(&key, &metadata)
		if err != nil {
			continue // Skip if error looking up this RMID
		}

		if metadata.Valid == 1 {
			// Convert []int8 to []byte for the comm field
			commBytes := make([]byte, len(metadata.Comm))
			for i, b := range metadata.Comm {
				commBytes[i] = byte(b)
			}
			// Convert comm to string, trimming null bytes
			comm := string(bytes.TrimRight(commBytes, "\x00"))
			log.Printf("%d\t%d\t%s\t%d\t%d\t%d\n",
				i, key, comm, metadata.Tgid, metadata.Timestamp, metadata.Valid)
		}
	}
	log.Println("") // Add blank line after dump
}

func main() {
	// Allow the current process to lock memory for eBPF resources
	if err := rlimit.RemoveMemlock(); err != nil {
		log.Fatal(err)
	}

	// Create parquet writer
	pw, err := newParquetWriter("metrics.parquet")
	if err != nil {
		log.Fatal(err)
	}
	defer func() {
		if err := pw.close(); err != nil {
			log.Printf("Error closing parquet writer: %v", err)
		}
	}()

	// Load pre-compiled programs and maps into the kernel
	objs := taskCounterObjects{}
	if err := loadTaskCounterObjects(&objs, nil); err != nil {
		log.Fatal(err)
	}
	defer objs.Close()

	// Attach the tracepoint programs
	tp, err := link.Tracepoint("memory_collector", "memory_collector_sample", objs.CountEvents, nil)
	if err != nil {
		log.Fatal(err)
	}
	defer tp.Close()

	// Attach RMID free tracepoint first, so we don't get dangling RMIDs
	rmidFreeTp, err := link.Tracepoint("memory_collector", "memory_collector_rmid_free", objs.HandleRmidFree, nil)
	if err != nil {
		log.Fatal(err)
	}
	defer rmidFreeTp.Close()

	// Attach RMID allocation tracepoint
	rmidAllocTp, err := link.Tracepoint("memory_collector", "memory_collector_rmid_alloc", objs.HandleRmidAlloc, nil)
	if err != nil {
		log.Fatal(err)
	}
	defer rmidAllocTp.Close()

	// Attach RMID existing tracepoint
	rmidExistingTp, err := link.Tracepoint("memory_collector", "memory_collector_rmid_existing", objs.HandleRmidExisting, nil)
	if err != nil {
		log.Fatal(err)
	}

	// Create a ReaderOptions with a large Watermark
	perCPUBufferSize := 16 * os.Getpagesize()
	opts := perf_ebpf.Options{
		BufferSize:     perCPUBufferSize,
		WatermarkBytes: uint32(perCPUBufferSize / 2),
	}

	// Create our perf map reader
	rd, err := perf_ebpf.NewPerfMapReader(objs.Events, opts)
	if err != nil {
		log.Fatal(err)
	}
	defer rd.Close()

	// Create the event openers for hardware counters
	commonOpts := unix.PerfEventAttr{
		Sample:      0,
		Sample_type: 0,
		Read_format: unix.PERF_FORMAT_TOTAL_TIME_ENABLED | unix.PERF_FORMAT_TOTAL_TIME_RUNNING,
		Bits:        0,
		Wakeup:      0,
		Bp_type:     0,
		Ext1:        0,
		Ext2:        0,
	}

	// Open cycles counter
	cyclesAttr := commonOpts
	cyclesAttr.Type = unix.PERF_TYPE_HARDWARE
	cyclesAttr.Config = unix.PERF_COUNT_HW_CPU_CYCLES
	cyclesOpener, err := NewEventOpener(objs.Cycles, cyclesAttr)
	if err != nil {
		log.Fatal(err)
	}
	defer cyclesOpener.Close()

	// Open instructions counter
	instrAttr := commonOpts
	instrAttr.Type = unix.PERF_TYPE_HARDWARE
	instrAttr.Config = unix.PERF_COUNT_HW_INSTRUCTIONS
	instrOpener, err := NewEventOpener(objs.Instructions, instrAttr)
	if err != nil {
		log.Fatal(err)
	}
	defer instrOpener.Close()

	// Open LLC misses counter
	llcAttr := commonOpts
	llcAttr.Type = unix.PERF_TYPE_HARDWARE
	llcAttr.Config = unix.PERF_COUNT_HW_CACHE_MISSES
	llcOpener, err := NewEventOpener(objs.LlcMisses, llcAttr)
	if err != nil {
		log.Fatal(err)
	}
	defer llcOpener.Close()

	// Trigger RMID dump via procfs
	if err := ioutil.WriteFile("/proc/unvariance_collector", []byte("dump"), 0644); err != nil {
		log.Fatal(err)
	}
	log.Println("Triggered RMID dump via procfs")

	// Close the RMID existing tracepoint since we're done with the dump
	if err := rmidExistingTp.Close(); err != nil {
		log.Printf("Warning: Failed to close RMID existing tracepoint: %v", err)
	}
	log.Println("Closed RMID existing tracepoint")

	// Dump RMID map after initial dump
	dumpRmidMap(&objs)

	// Catch CTRL+C
	stopper := make(chan os.Signal, 1)
	signal.Notify(stopper, os.Interrupt)

	timeout := time.After(5 * time.Second)
	ticker := time.NewTicker(100 * time.Millisecond)
	defer ticker.Stop()

	// Counter to maintain in userspace
	var totalEvents uint64 = 0

	// Start the reader
	reader := rd.Reader()

	log.Println("Waiting for events...")

	// Create aggregator with 100ms slots and 10 slots window
	aggregatorConfig := aggregate.Config{
		SlotLength: 10_000_000, // 10ms in nanoseconds
		WindowSize: 10,         // Keep 10 slots (100ms total)
		SlotOffset: 0,
	}
	aggregator, err := aggregate.NewAggregator(aggregatorConfig)
	if err != nil {
		log.Fatal(err)
	}

	// Helper function to write completed time slots to parquet
	writeCompletedSlots := func(slots []*aggregate.TimeSlot) {
		if err := pw.writeTimeSlots(slots); err != nil {
			log.Printf("Error writing time slots to parquet: %v", err)
		}
	}

	for {
		select {
		case <-stopper:
			log.Printf("Received interrupt, exiting... Total events: %d\n", totalEvents)
			// Write any remaining slots before exiting
			writeCompletedSlots(aggregator.Reset())
			dumpRmidMap(&objs) // Dump RMID map before exiting
			return
		case <-timeout:
			log.Println("Finished counting after 5 seconds")
			// Write any remaining slots before exiting
			writeCompletedSlots(aggregator.Reset())
			dumpRmidMap(&objs) // Dump RMID map before exiting
			return
		case <-ticker.C:
			// Get current monotonic timestamp before starting the batch
			startTimestamp := uint64(nanotime())

			log.Printf("Starting batch at timestamp: %d", startTimestamp)

			if err := reader.Start(); err != nil {
				log.Fatal(err)
			}

			// Process all available events that occurred before startTimestamp
			for !reader.Empty() {
				// Check if next event's timestamp is after our start timestamp
				ts, err := reader.PeekTimestamp()
				if err != nil {
					log.Printf("Error peeking timestamp: %s", err)
					break
				}

				// Skip processing this batch if we see an event from the future
				if ts > startTimestamp {
					break
				}

				ring, cpuID, err := reader.CurrentRing()
				if err != nil {
					log.Printf("Error getting current ring: %s", err)
					break
				}

				// Check for lost samples
				if ring.PeekType() == ourperf.PERF_RECORD_LOST {
					var lostCount uint64
					if err := ring.PeekCopy((*[8]byte)(unsafe.Pointer(&lostCount))[:], 8); err != nil {
						log.Printf("Error reading lost count: %s", err)
					} else {
						log.Printf("Lost %d samples on CPU %d", lostCount, cpuID)
					}
					reader.Pop()
					continue
				}

				// Parse the raw event
				size, err := ring.PeekSize()
				if err != nil {
					log.Printf("Error getting event size: %s", err)
					break
				}

				eventData := make([]byte, size-4)
				if err := ring.PeekCopy(eventData, 4); err != nil {
					log.Printf("Error copying event data: %s", err)
					break
				}

				var event taskCounterEvent
				if err := binary.Read(bytes.NewReader(eventData), binary.LittleEndian, &event); err != nil {
					log.Printf("Failed to parse perf event: %s", err)
					break
				}

				// Create measurement from event
				measurement := &aggregate.Measurement{
					RMID:         event.Rmid,
					Cycles:       event.CyclesDelta,
					Instructions: event.InstructionsDelta,
					LLCMisses:    event.LlcMissesDelta,
					Timestamp:    event.Timestamp,
					Duration:     event.TimeDeltaNs,
				}

				// Advance window and write any completed slots
				if completedSlots := aggregator.AdvanceWindow(event.Timestamp, event.TimeDeltaNs); len(completedSlots) > 0 {
					writeCompletedSlots(completedSlots)
				}

				// Update aggregator with the measurement
				if err := aggregator.UpdateMeasurement(measurement); err != nil {
					log.Printf("Error updating aggregator: %s", err)
				}

				totalEvents++
				reader.Pop()
			}

			if err := reader.Finish(); err != nil {
				log.Printf("Error finishing reader: %s", err)
			}

			// Output counts every second
			var count uint64
			var key uint32 = 0
			if err := objs.EventCount.Lookup(&key, &count); err != nil {
				log.Fatal(err)
			}
			log.Printf("Event count: userspace %d, eBPF %d\n", totalEvents, count)
		}
	}
}
