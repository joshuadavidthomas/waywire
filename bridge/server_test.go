package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/gorilla/websocket"
)

const testOrigin = "https://desk.example.test"

func testBridge(dial socketDialer) *bridgeServer {
	cfg := config{Origin: testOrigin, SocketPath: "/tmp/rfb.sock", PingInterval: time.Hour}
	version := versionDocument{Release: "v1.2.3", Source: "abc", Installed: installedMetadata{Release: "v1.2.3", ObservedAt: time.Now(), Packages: map[string]string{}}}
	return newBridgeServer(cfg, version, dial, filepath.Join(os.TempDir(), "missing-task.sock"))
}

func TestRouteContractAndHeaders(t *testing.T) {
	dial := func(context.Context) (net.Conn, error) { return nil, os.ErrNotExist }
	s := testBridge(dial)
	assets, err := webFiles.ReadDir("web/assets")
	if err != nil || len(assets) == 0 {
		t.Fatal("build the viewer assets before testing the bridge")
	}
	assetPath := "/assets/" + assets[0].Name()
	tests := []struct {
		method string
		path   string
		status int
		cache  string
	}{
		{http.MethodGet, "/", 200, "no-store"},
		{http.MethodHead, "/", 200, "no-store"},
		{http.MethodGet, assetPath, 200, "public, max-age=31536000, immutable"},
		{http.MethodHead, "/healthz", 200, "no-store"},
		{http.MethodGet, "/version", 200, "no-store"},
		{http.MethodPost, "/", 405, ""},
		{http.MethodPost, "/healthz", 405, ""},
		{http.MethodGet, "/vnc", 426, ""},
		{http.MethodGet, "/missing", 404, ""},
		{http.MethodGet, "/assets/../index.html", 404, ""},
		{http.MethodGet, "/assets/not-present.js", 404, ""},
	}
	for _, tc := range tests {
		t.Run(tc.method+tc.path, func(t *testing.T) {
			recorder := httptest.NewRecorder()
			s.handler().ServeHTTP(recorder, httptest.NewRequest(tc.method, tc.path, nil))
			if recorder.Code != tc.status {
				t.Fatalf("status = %d, want %d", recorder.Code, tc.status)
			}
			if tc.cache != "" && recorder.Header().Get("Cache-Control") != tc.cache {
				t.Fatalf("Cache-Control = %q", recorder.Header().Get("Cache-Control"))
			}
			for header, want := range map[string]string{
				"Content-Security-Policy": contentSecurityPolicy,
				"X-Content-Type-Options":  "nosniff",
				"Referrer-Policy":         "no-referrer",
			} {
				if got := recorder.Header().Get(header); got != want {
					t.Errorf("%s = %q, want %q", header, got, want)
				}
			}
		})
	}
}

func TestWebsocketOriginMatrix(t *testing.T) {
	dial := func(context.Context) (net.Conn, error) { return nil, os.ErrNotExist }
	httpServer := httptest.NewServer(testBridge(dial).handler())
	defer httpServer.Close()
	wsURL := "ws" + httpServer.URL[len("http"):] + "/vnc"
	origins := [][]string{
		nil,
		{"null"},
		{"%%%"},
		{"http://desk.example.test"},
		{"https://other.example.test"},
		{"https://desk.example.test:443"},
		{testOrigin + ", https://other.example.test"},
		{testOrigin, "https://other.example.test"},
	}
	for _, values := range origins {
		header := http.Header{}
		header["Origin"] = values
		conn, response, err := websocket.DefaultDialer.Dial(wsURL, header)
		if conn != nil {
			conn.Close()
		}
		if err == nil || response == nil || response.StatusCode != http.StatusForbidden {
			t.Fatalf("origins %q: err=%v status=%v", values, err, responseStatus(response))
		}
	}
}

func responseStatus(response *http.Response) any {
	if response == nil {
		return nil
	}
	return response.StatusCode
}

func TestWebsocketUnixRelayPreservesFragmentedBinary(t *testing.T) {
	listener, path := unixListener(t)
	defer listener.Close()
	go func() {
		conn, err := listener.Accept()
		if err == nil {
			defer conn.Close()
			io.Copy(conn, conn)
		}
	}()
	s := testBridge(unixDial(path))
	httpServer := httptest.NewServer(s.handler())
	defer httpServer.Close()
	ws := dialWebsocket(t, httpServer.URL)
	defer ws.Close()

	payload := bytes.Repeat([]byte{0, 1, 2, 3, 255}, 30_000)
	writer, err := ws.NextWriter(websocket.BinaryMessage)
	if err != nil {
		t.Fatal(err)
	}
	for offset := 0; offset < len(payload); offset += 997 {
		end := min(offset+997, len(payload))
		if _, err := writer.Write(payload[offset:end]); err != nil {
			t.Fatal(err)
		}
	}
	if err := writer.Close(); err != nil {
		t.Fatal(err)
	}
	var received []byte
	for len(received) < len(payload) {
		kind, part, err := ws.ReadMessage()
		if err != nil {
			t.Fatal(err)
		}
		if kind != websocket.BinaryMessage {
			t.Fatalf("message type = %d", kind)
		}
		received = append(received, part...)
	}
	if !bytes.Equal(received, payload) {
		t.Fatal("relay changed binary bytes")
	}
}

func TestTextMessageAndMissedPongsCleanUpViewer(t *testing.T) {
	t.Run("rfb-side", func(t *testing.T) {
		listener, path := unixListener(t)
		defer listener.Close()
		go func() {
			conn, err := listener.Accept()
			if err == nil {
				conn.Close()
			}
		}()
		s := testBridge(unixDial(path))
		httpServer := httptest.NewServer(s.handler())
		defer httpServer.Close()
		ws := dialWebsocket(t, httpServer.URL)
		defer ws.Close()
		if _, _, err := ws.ReadMessage(); err == nil {
			t.Fatal("RFB EOF did not close the WebSocket")
		}
		waitFor(t, time.Second, func() bool { return s.viewers.Load() == 0 })
	})
	t.Run("text", func(t *testing.T) {
		listener, path := unixListener(t)
		defer listener.Close()
		go acceptAndHold(listener)
		s := testBridge(unixDial(path))
		httpServer := httptest.NewServer(s.handler())
		defer httpServer.Close()
		ws := dialWebsocket(t, httpServer.URL)
		waitFor(t, time.Second, func() bool { return s.viewers.Load() == 1 })
		if err := ws.WriteMessage(websocket.TextMessage, []byte("not rfb")); err != nil {
			t.Fatal(err)
		}
		waitFor(t, time.Second, func() bool { return s.viewers.Load() == 0 })
		ws.Close()
	})
	t.Run("pongs", func(t *testing.T) {
		listener, path := unixListener(t)
		defer listener.Close()
		go acceptAndHold(listener)
		s := testBridge(unixDial(path))
		s.config.PingInterval = 40 * time.Millisecond
		s.pongTimeout = 20 * time.Millisecond
		httpServer := httptest.NewServer(s.handler())
		defer httpServer.Close()
		ws := dialWebsocket(t, httpServer.URL)
		defer ws.Close()
		waitFor(t, time.Second, func() bool { return s.viewers.Load() == 1 })
		waitFor(t, time.Second, func() bool { return s.viewers.Load() == 0 })
	})
}

func TestRFBReadinessHandshakeAndFailure(t *testing.T) {
	listener, path := unixListener(t)
	defer listener.Close()
	go serveRFBHandshake(listener)
	if err := probeRFB(context.Background(), unixDial(path)); err != nil {
		t.Fatalf("valid handshake: %v", err)
	}

	bad, badPath := unixListener(t)
	defer bad.Close()
	go func() {
		conn, err := bad.Accept()
		if err == nil {
			conn.Write([]byte("not an rfb!!"))
			conn.Close()
		}
	}()
	if err := probeRFB(context.Background(), unixDial(badPath)); err == nil {
		t.Fatal("invalid RFB server reported ready")
	}
}

func TestTaskOperationsAreSerializedAndTrackCurrentViewerCount(t *testing.T) {
	listener, path := unixListener(t)
	var mu sync.Mutex
	var methods []string
	var inFlight atomic.Int64
	var maxInFlight atomic.Int64
	server := &http.Server{Handler: http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		current := inFlight.Add(1)
		defer inFlight.Add(-1)
		for current > maxInFlight.Load() && !maxInFlight.CompareAndSwap(maxInFlight.Load(), current) {
		}
		if r.URL.Path != "/v1/tasks/sprite-desktop-viewer" {
			t.Errorf("task path = %q", r.URL.Path)
		}
		if r.Method == http.MethodPut {
			var body map[string]string
			json.NewDecoder(r.Body).Decode(&body)
			if body["expire"] != "90s" {
				t.Errorf("task expiry = %q", body["expire"])
			}
		}
		mu.Lock()
		methods = append(methods, r.Method)
		mu.Unlock()
		w.WriteHeader(http.StatusNoContent)
	})}
	go server.Serve(listener)
	defer server.Close()

	var viewers atomic.Int64
	manager := newTaskManager(true, &viewers, path)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	go manager.run(ctx)
	viewers.Store(1)
	manager.changed()
	waitFor(t, time.Second, manager.held.Load)
	viewers.Store(0)
	manager.changed()
	waitFor(t, time.Second, func() bool {
		mu.Lock()
		defer mu.Unlock()
		return len(methods) >= 2 && methods[len(methods)-1] == http.MethodDelete
	})
	if maxInFlight.Load() != 1 {
		t.Fatalf("max concurrent task requests = %d", maxInFlight.Load())
	}
}

func TestCheckConfigDoesNotReadVersionOrStartNetwork(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.json")
	data := `{"origin":"https://desk.example.test","socket_path":"/tmp/rfb.sock","ping_interval":"20s","keepalive":{"task":false}}`
	if err := os.WriteFile(path, []byte(data), 0600); err != nil {
		t.Fatal(err)
	}
	if err := run([]string{"--config", path, "--check-config"}); err != nil {
		t.Fatal(err)
	}
	if err := run([]string{"--config", path + ".missing", "--check-config"}); err == nil {
		t.Fatal("missing config passed validation")
	}

	for index, origin := range []string{"", "http://desk.example.test", "https://desk.example.test/path", "https://desk.example.test:443", "https://user@desk.example.test"} {
		invalidPath := filepath.Join(t.TempDir(), fmt.Sprintf("invalid-%d.json", index))
		invalid := fmt.Sprintf(`{"origin":%q}`, origin)
		if err := os.WriteFile(invalidPath, []byte(invalid), 0600); err != nil {
			t.Fatal(err)
		}
		if err := run([]string{"--config", invalidPath, "--check-config"}); err == nil {
			t.Errorf("invalid origin %q passed validation", origin)
		}
	}
}

func unixListener(t *testing.T) (net.Listener, string) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "socket")
	listener, err := net.Listen("unix", path)
	if err != nil {
		t.Fatal(err)
	}
	return listener, path
}

func unixDial(path string) socketDialer {
	return func(ctx context.Context) (net.Conn, error) {
		return (&net.Dialer{}).DialContext(ctx, "unix", path)
	}
}

func dialWebsocket(t *testing.T, serverURL string) *websocket.Conn {
	t.Helper()
	u, _ := url.Parse(serverURL)
	u.Scheme = "ws"
	u.Path = "/vnc"
	header := http.Header{"Origin": []string{testOrigin}}
	conn, response, err := websocket.DefaultDialer.Dial(u.String(), header)
	if err != nil {
		t.Fatalf("dial websocket: %v (status %v)", err, responseStatus(response))
	}
	conn.SetReadDeadline(time.Now().Add(5 * time.Second))
	return conn
}

func acceptAndHold(listener net.Listener) {
	conn, err := listener.Accept()
	if err == nil {
		defer conn.Close()
		io.Copy(io.Discard, conn)
	}
}

func serveRFBHandshake(listener net.Listener) {
	conn, err := listener.Accept()
	if err != nil {
		return
	}
	defer conn.Close()
	conn.Write([]byte("RFB 003.008\n"))
	version := make([]byte, 12)
	io.ReadFull(conn, version)
	conn.Write([]byte{1, 1})
	selection := make([]byte, 1)
	io.ReadFull(conn, selection)
	conn.Write([]byte{0, 0, 0, 0})
	io.ReadFull(conn, selection)
	header := make([]byte, 24)
	binary.BigEndian.PutUint16(header[0:2], 1280)
	binary.BigEndian.PutUint16(header[2:4], 720)
	binary.BigEndian.PutUint32(header[20:24], 4)
	conn.Write(header)
	conn.Write([]byte("test"))
}

func waitFor(t *testing.T, timeout time.Duration, condition func() bool) {
	t.Helper()
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if condition() {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatal("condition was not met before timeout")
}
