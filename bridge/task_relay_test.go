package main

import (
	"context"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"testing"
	"time"
)

func TestRelayReleasesTaskAfterEitherEndpointCloses(t *testing.T) {
	for _, scenario := range []string{"browser-close", "rfb-close", "failed-put"} {
		t.Run(scenario, func(t *testing.T) {
			apiListener, apiPath := unixListener(t)
			methods := make(chan string, 8)
			api := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				methods <- r.Method
				if scenario == "failed-put" && r.Method == http.MethodPut {
					w.WriteHeader(http.StatusServiceUnavailable)
				} else {
					w.WriteHeader(http.StatusNoContent)
				}
			})}
			go api.Serve(apiListener)
			defer api.Close()
			listener, path := unixListener(t)
			defer listener.Close()
			peer := make(chan net.Conn, 1)
			peerClosed := make(chan struct{})
			go func() {
				conn, err := listener.Accept()
				if err != nil {
					return
				}
				defer conn.Close()
				peer <- conn
				io.Copy(io.Discard, conn)
				close(peerClosed)
			}()
			bridge := testBridge(unixDial(path))
			bridge.tasks = newTaskManager(true, &bridge.viewers, apiPath)
			ctx, cancel := context.WithCancel(context.Background())
			taskDone := make(chan struct{})
			go func() { defer close(taskDone); bridge.tasks.run(ctx) }()
			defer func() { cancel(); <-taskDone }()
			server := httptest.NewServer(bridge.handler())
			defer server.Close()
			ws := dialWebsocket(t, server.URL)
			defer ws.Close()
			var conn net.Conn
			select {
			case conn = <-peer:
			case <-time.After(time.Second):
				t.Fatal("RFB was not attached")
			}
			select {
			case method := <-methods:
				if method != http.MethodPut {
					t.Fatalf("first task mutation = %s", method)
				}
			case <-time.After(time.Second):
				t.Fatal("viewer did not acquire a task")
			}
			if scenario == "rfb-close" {
				conn.Close()
			} else {
				ws.Close()
			}
			select {
			case <-peerClosed:
			case <-time.After(time.Second):
				t.Fatal("RFB connection leaked")
			}
			select {
			case method := <-methods:
				if method != http.MethodDelete {
					t.Fatalf("detach task mutation = %s", method)
				}
				if bridge.viewers.Load() != 0 || bridge.active.Load() != 0 {
					t.Fatal("task deleted before relay cleanup")
				}
			case <-time.After(time.Second):
				t.Fatal("detach did not release task")
			}
		})
	}
}
