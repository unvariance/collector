package main

import (
	"bytes"
	"encoding/binary"
	"errors"
	"log"
	"os"
	"os/signal"
	"time"

	"github.com/cilium/ebpf/link"
	"github.com/cilium/ebpf/perf"
	"github.com/cilium/ebpf/rlimit"
	"golang.org/x/sys/unix"
)

// Note: taskCounterEvent is auto-generated by bpf2go

func main() {
	// Allow the current process to lock memory for eBPF resources
	if err := rlimit.RemoveMemlock(); err != nil {
		log.Fatal(err)
	}

	// Load pre-compiled programs and maps into the kernel
	objs := taskCounterObjects{}
	if err := loadTaskCounterObjects(&objs, nil); err != nil {
		log.Fatal(err)
	}
	defer objs.Close()

	// Attach the tracepoint program
	tp, err := link.Tracepoint("memory_collector", "memory_collector_sample", objs.CountEvents, nil)
	if err != nil {
		log.Fatal(err)
	}
	defer tp.Close()

	// Create a ReaderOptions with a large Watermark
	perCPUBufferSize := 16 * os.Getpagesize()
	opts := perf.ReaderOptions{
		Watermark: perCPUBufferSize / 2,
	}

	// Open a perf reader from userspace
	rd, err := perf.NewReaderWithOptions(objs.Events, perCPUBufferSize, opts)
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

	// Catch CTRL+C
	stopper := make(chan os.Signal, 1)
	signal.Notify(stopper, os.Interrupt)

	timeout := time.After(5 * time.Second)

	// set deadline in the past for rd, so it will not block
	nextDeadline := time.Now().Add(time.Second)
	rd.SetDeadline(nextDeadline)

	log.Println("Waiting for events...")

	// Counter to maintain in userspace
	var totalEvents uint64 = 0

	for {
		select {
		case <-stopper:
			log.Printf("Received interrupt, exiting... Total events: %d\n", totalEvents)
			return
		case <-timeout:
			log.Println("Finished counting after 5 seconds")
			return
		default:

			// if the deadline is in the past, set it to the next deadline
			if time.Now().After(nextDeadline) {
				nextDeadline = nextDeadline.Add(time.Second)
				rd.SetDeadline(nextDeadline)

				// output counts
				var count uint64
				var key uint32 = 0
				if err := objs.EventCount.Lookup(&key, &count); err != nil {
					log.Fatal(err)
				}
				log.Printf("Event count: userspace %d, eBPF %d\n", totalEvents, count)
			}

			record, err := rd.Read()
			if err != nil {
				if errors.Is(err, os.ErrDeadlineExceeded) || errors.Is(err, perf.ErrFlushed) {					
					break // make for loop check the select statement and set the deadline
				} else if errors.Is(err, perf.ErrClosed) {
					return
				}
				log.Printf("Reading from perf event reader: %s", err)
				continue
			}

			if record.LostSamples != 0 {
				log.Printf("Lost %d samples", record.LostSamples)
				continue
			}

			// Parse the raw bytes into our Event struct
			var event taskCounterEvent
			if err := binary.Read(bytes.NewReader(record.RawSample), binary.LittleEndian, &event); err != nil {
				log.Printf("Failed to parse perf event: %s", err)
				continue
			}

			log.Printf("Event - CPU: %d, Cycles: %d, Instructions: %d, LLC Misses: %d", 
				record.CPU, event.Cycles, event.Instructions, event.LlcMisses)
			totalEvents++
		}
	}
}