package main

import (
	"bytes"
	"context"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"
)

func TestCountingWriterWriteDeadlineWhenPaused(t *testing.T) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer ln.Close()

	readerCount := 1
	handlerDone := make(chan struct{})

	// 10 MB payload so that write buffer fills up when client pauses
	payload := bytes.Repeat([]byte("0123456789abcdefghijklmnopqrstuv"), 320*1024)

	srv := &http.Server{
		Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			defer func() {
				readerCount--
				close(handlerDone)
			}()

			rc := http.NewResponseController(w)
			cw := &countingWriter{
				ResponseWriter: w,
				rc:             rc,
				timeout: func() time.Duration {
					return 300 * time.Millisecond
				},
			}
			sr := &sizedReader{
				r:    bytes.NewReader(payload),
				size: int64(len(payload)),
			}
			http.ServeContent(cw, r, "video.mp4", time.Now(), sr)
		}),
	}
	go srv.Serve(ln)
	defer srv.Shutdown(context.Background())

	conn, err := net.Dial("tcp", ln.Addr().String())
	if err != nil {
		t.Fatal(err)
	}
	defer conn.Close()

	_, err = conn.Write([]byte("GET /video.mp4 HTTP/1.1\r\nHost: localhost\r\nRange: bytes=0-\r\n\r\n"))
	if err != nil {
		t.Fatal(err)
	}

	// Read small chunk to start the stream
	buf := make([]byte, 4096)
	n, err := conn.Read(buf)
	if err != nil || n == 0 {
		t.Fatalf("failed to read response: %v", err)
	}

	if readerCount != 1 {
		t.Fatalf("expected 1 reader, got %d", readerCount)
	}

	// Client now pauses (stops reading from socket, mimicking TV being paused/shut off).
	// Handler should time out on write deadline and readerCount should drop to 0.
	select {
	case <-handlerDone:
		if readerCount != 0 {
			t.Fatalf("expected readerCount 0 after timeout, got %d", readerCount)
		}
	case <-time.After(3 * time.Second):
		t.Fatal("handler did not exit on paused client during ServeContent")
	}
}

func TestCountingWriterSurvivesNormalPause(t *testing.T) {
	payload := bytes.Repeat([]byte("0123456789abcdefghijklmnopqrstuv"), 32*1024) // 1MB

	ts := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		rc := http.NewResponseController(w)
		cw := &countingWriter{
			ResponseWriter: w,
			rc:             rc,
			timeout: func() time.Duration {
				return 500 * time.Millisecond
			},
		}
		sr := &sizedReader{
			r:    bytes.NewReader(payload),
			size: int64(len(payload)),
		}
		http.ServeContent(cw, r, "video.mp4", time.Now(), sr)
	}))
	defer ts.Close()

	req, _ := http.NewRequest("GET", ts.URL+"/video.mp4", nil)
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()

	// Read small chunk
	buf := make([]byte, 8192)
	n, err := io.ReadFull(resp.Body, buf)
	if err != nil || n != 8192 {
		t.Fatalf("failed reading: %v", err)
	}

	// Pause for 150ms (< 500ms timeout)
	time.Sleep(150 * time.Millisecond)

	// Read the rest
	rest, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatalf("read after pause failed: %v", err)
	}
	if len(buf)+len(rest) != len(payload) {
		t.Fatalf("expected %d bytes, got %d", len(payload), len(buf)+len(rest))
	}
}

func TestCountingWriterTouchesAccess(t *testing.T) {
	var accessed atomic.Bool
	w := httptest.NewRecorder()
	cw := &countingWriter{
		ResponseWriter: w,
		onAccess: func() {
			accessed.Store(true)
		},
	}
	n, err := cw.Write([]byte("hello"))
	if err != nil || n != 5 {
		t.Fatalf("unexpected write result: %d, %v", n, err)
	}
	if !accessed.Load() {
		t.Fatal("expected onAccess to be called on write")
	}
	if cw.Unwrap() != w {
		t.Fatal("expected Unwrap() to return underlying ResponseWriter")
	}
}
