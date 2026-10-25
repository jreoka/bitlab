package main

import (
	"io"
	"log"
	"strings"
	"testing"
	"time"
)

// Stream-first: background warming must yield whenever a play reader is
// open, and resume once the last reader closes.

func testEngineWithActivity() *Engine {
	return &Engine{
		act:     map[string]*activity{},
		warmSem: make(chan struct{}, 4),
		log:     log.New(io.Discard, "", 0),
	}
}

func TestStreamBusyTracksReaders(t *testing.T) {
	e := testEngineWithActivity()
	ih := strings.Repeat("a", 40)
	if e.streamBusy() {
		t.Fatal("no activity yet: streamBusy should be false")
	}
	a := e.activity(ih)
	if e.streamBusy() {
		t.Fatal("activity without readers: streamBusy should be false")
	}
	e.mu.Lock()
	a.readers++
	e.mu.Unlock()
	if !e.streamBusy() {
		t.Fatal("one open play reader: streamBusy should be true")
	}
	e.mu.Lock()
	a.readers--
	e.mu.Unlock()
	if e.streamBusy() {
		t.Fatal("reader closed: streamBusy should be false again")
	}
}

func TestWarmSkipsWhileStreamBusy(t *testing.T) {
	e := testEngineWithActivity()
	ih := strings.Repeat("b", 40)
	a := e.activity(ih)
	e.mu.Lock()
	a.readers = 1
	e.mu.Unlock()

	// With a stream active, Warm must return at the gate: it must not
	// consume a warm slot, and it must not reach ensure() (which would
	// need a live torrent client and would panic on this bare Engine).
	done := make(chan struct{})
	go func() {
		defer close(done)
		e.Warm(ih, "some release", nil, 8)
	}()
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("Warm did not return while a stream was busy")
	}
	if len(e.warmSem) != 0 {
		t.Fatal("Warm consumed a warm slot despite the busy gate")
	}
}

func TestWarmInvalidHashStillNoop(t *testing.T) {
	e := testEngineWithActivity()
	e.Warm("not-a-hash", "x", nil, 8) // must simply return
}
