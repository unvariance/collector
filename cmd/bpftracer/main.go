package main

import (
	"fmt"
	"os"
	"os/exec"
	// "os/signal"
	// "syscall"
	"time"
)

func main() {
	// Path to bpftrace script
	scriptPath := "./unvariance_bpftracer.bt"

	// Command to run the bpftrace script
	cmd := exec.Command("sudo", "bpftrace", scriptPath)

	// Set up stdout and stderr
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	if err := cmd.Start(); err != nil {
		fmt.Printf("Failed to start bpftrace: %v\n", err)
		return
	}

	// Set up signal handling to stop the command gracefully
	// sig := make(chan os.Signal, 1)
	// signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)

	// Wait for the command to finish or for a signal to be received
	go func() {
		if err := cmd.Wait(); err != nil {
			fmt.Printf("bpftrace exited with error: %v\n", err)
		}
	}()

	// Wait for a set time
	fmt.Println("Running bpftrace for set time...")
	time.Sleep(1000 * time.Millisecond)

	// Kill the bpftrace process
	fmt.Println("Stopping bpftrace...")
	if err := cmd.Process.Kill(); err != nil {
		fmt.Printf("Failed to kill bpftrace: %v\n", err)
	}

	// // Wait for a signal
	// <-sig
	// fmt.Println("Received signal, stopping bpftrace...")
	//
	// // Kill the bpftrace process
	// if err := cmd.Process.Kill(); err != nil {
	// fmt.Printf("Failed to kill bpftrace: %v\n", err)
	// }
}
