package main

import (
	"context"
	"io"
	"net"
	"net/http/httptest"
	"sync/atomic"
	"testing"
	"time"
)

func TestReadinessCachesCompletedHandshake(t *testing.T) {
	listener, path := unixListener(t)
	defer listener.Close()
	go serveRFBHandshake(listener)
	var viewers, active atomic.Int64
	readiness := rfbReadiness{dial: unixDial(path), viewers: &viewers, active: &active}
	if !readiness.status(context.Background()) {
		t.Fatal("full handshake did not establish readiness")
	}
	// With the listener closed, a second dial would fail. Cached health remains
	// available during the ten-second window without opening another RFB login.
	listener.Close()
	for i := 0; i < 1000; i++ {
		if !readiness.status(context.Background()) {
			t.Fatal("health opened another connection inside its cache window")
		}
	}
}

func TestViewerArrivingDuringReadinessProbe(t *testing.T) {
	listener, path := unixListener(t)
	defer listener.Close()
	go serveRFBHandshake(listener)
	started, proceed := make(chan struct{}), make(chan struct{})
	var viewers, active atomic.Int64
	readiness := rfbReadiness{
		viewers: &viewers, active: &active,
		dial: func(ctx context.Context) (net.Conn, error) {
			close(started)
			select {
			case <-proceed:
				return unixDial(path)(ctx)
			case <-ctx.Done():
				return nil, ctx.Err()
			}
		},
	}
	result := make(chan bool, 1)
	go func() { result <- readiness.status(context.Background()) }()
	select {
	case <-started:
	case <-time.After(time.Second):
		t.Fatal("readiness probe did not start")
	}
	viewers.Store(1)
	active.Store(1)
	close(proceed)
	select {
	case ready := <-result:
		if !ready {
			t.Fatal("readiness ignored the viewer that arrived during its handshake")
		}
	case <-time.After(4 * time.Second):
		t.Fatal("readiness probe did not finish")
	}
}

func TestCancellationClosesBothRelayDirections(t *testing.T) {
	listener, path := unixListener(t)
	defer listener.Close()
	peerClosed := make(chan struct{})
	go func() {
		conn, err := listener.Accept()
		if err != nil {
			return
		}
		defer conn.Close()
		io.Copy(io.Discard, conn)
		close(peerClosed)
	}()
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	bridge := testBridge(unixDial(path))
	server := httptest.NewUnstartedServer(bridge.handler())
	server.Config.BaseContext = func(net.Listener) context.Context { return ctx }
	server.Start()
	defer server.Close()
	ws := dialWebsocket(t, server.URL)
	defer ws.Close()
	waitFor(t, time.Second, func() bool { return bridge.active.Load() == 1 })
	cancel()
	if _, _, err := ws.ReadMessage(); err == nil {
		t.Fatal("cancelled relay remained open")
	}
	select {
	case <-peerClosed:
	case <-time.After(time.Second):
		t.Fatal("RFB reader was left blocked")
	}
	waitFor(t, time.Second, func() bool { return bridge.viewers.Load() == 0 && bridge.active.Load() == 0 })
}

func TestLiveViewerDoesNotStartHealthProbe(t *testing.T) {
	listener, path := unixListener(t)
	defer listener.Close()
	go acceptAndHold(listener)
	bridge := testBridge(unixDial(path))
	server := httptest.NewServer(bridge.handler())
	defer server.Close()
	ws := dialWebsocket(t, server.URL)
	defer ws.Close()
	waitFor(t, time.Second, func() bool { return bridge.active.Load() == 1 })
	// Retire the listening socket while the established RFB connection remains.
	listener.Close()
	for i := 0; i < 1000; i++ {
		if !bridge.readiness.status(context.Background()) {
			t.Fatal("health tried a fresh login despite a live viewer")
		}
	}
}
