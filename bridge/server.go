package main

import (
	"context"
	"embed"
	"encoding/json"
	"errors"
	"io"
	"mime"
	"net"
	"net/http"
	"path/filepath"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/gorilla/websocket"
)

//go:embed web
var webFiles embed.FS

const contentSecurityPolicy = "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'"

type bridgeServer struct {
	config      config
	version     versionDocument
	dial        socketDialer
	sessions    sync.WaitGroup
	viewers     atomic.Int64
	active      atomic.Int64
	tasks       *taskManager
	readiness   *rfbReadiness
	pongTimeout time.Duration
}

func newBridgeServer(cfg config, version versionDocument, dial socketDialer, taskSocket string) *bridgeServer {
	s := &bridgeServer{config: cfg, version: version, dial: dial, pongTimeout: 15 * time.Second}
	s.tasks = newTaskManager(cfg.TaskHold, &s.viewers, taskSocket)
	s.readiness = &rfbReadiness{dial: dial, viewers: &s.viewers, active: &s.active}
	return s
}

func (s *bridgeServer) handler() http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		setSecurityHeaders(w)
		switch r.URL.Path {
		case "/":
			s.serveEmbedded(w, r, "web/index.html", "text/html; charset=utf-8", "no-store")
		case "/healthz":
			s.serveHealth(w, r)
		case "/version":
			s.serveJSON(w, r, s.version)
		case "/vnc":
			s.serveVNC(w, r)
		default:
			if strings.HasPrefix(r.URL.Path, "/assets/") {
				s.serveEmbedded(w, r, "web"+r.URL.Path, "", "public, max-age=31536000, immutable")
			} else {
				http.NotFound(w, r)
			}
		}
	})
}

func setSecurityHeaders(w http.ResponseWriter) {
	w.Header().Set("Content-Security-Policy", contentSecurityPolicy)
	w.Header().Set("X-Content-Type-Options", "nosniff")
	w.Header().Set("Referrer-Policy", "no-referrer")
}

func allowedReadMethod(w http.ResponseWriter, r *http.Request) bool {
	if r.Method == http.MethodGet || r.Method == http.MethodHead {
		return true
	}
	w.Header().Set("Allow", "GET, HEAD")
	http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
	return false
}

func (s *bridgeServer) serveEmbedded(w http.ResponseWriter, r *http.Request, path, contentType, cache string) {
	data, err := webFiles.ReadFile(path)
	if err != nil {
		http.NotFound(w, r)
		return
	}
	if !allowedReadMethod(w, r) {
		return
	}
	if contentType == "" {
		contentType = mime.TypeByExtension(filepath.Ext(path))
		if contentType == "" {
			contentType = "application/octet-stream"
		}
	}
	w.Header().Set("Content-Type", contentType)
	w.Header().Set("Cache-Control", cache)
	if r.Method != http.MethodHead {
		_, _ = w.Write(data)
	}
}

func (s *bridgeServer) serveJSON(w http.ResponseWriter, r *http.Request, value any) {
	if !allowedReadMethod(w, r) {
		return
	}
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Cache-Control", "no-store")
	if r.Method != http.MethodHead {
		_ = json.NewEncoder(w).Encode(value)
	}
}

func (s *bridgeServer) serveHealth(w http.ResponseWriter, r *http.Request) {
	if !allowedReadMethod(w, r) {
		return
	}
	rfb := "starting"
	if s.readiness.status(r.Context()) {
		rfb = "listening"
	}
	s.serveJSON(w, r, struct {
		Release  string    `json:"release"`
		Bridge   string    `json:"bridge"`
		RFB      string    `json:"rfb"`
		Attached int64     `json:"attached"`
		TaskHeld bool      `json:"task_held"`
		Checked  time.Time `json:"checked_at"`
	}{s.version.Release, "ok", rfb, s.viewers.Load(), s.tasks.held.Load(), time.Now().UTC()})
}

func (s *bridgeServer) serveVNC(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet {
		w.Header().Set("Allow", "GET")
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	if !websocket.IsWebSocketUpgrade(r) {
		http.Error(w, "websocket upgrade required", http.StatusUpgradeRequired)
		return
	}
	origins := r.Header.Values("Origin")
	if len(origins) != 1 || origins[0] != s.config.Origin {
		http.Error(w, "forbidden origin", http.StatusForbidden)
		return
	}
	s.sessions.Add(1)
	defer s.sessions.Done()
	if r.Context().Err() != nil {
		return
	}
	upgrader := websocket.Upgrader{CheckOrigin: func(*http.Request) bool { return true }}
	ws, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	s.relay(r.Context(), ws)
}

func (s *bridgeServer) relay(parent context.Context, ws *websocket.Conn) {
	ctx, cancel := context.WithCancel(parent)
	var socket net.Conn
	var workers sync.WaitGroup
	if s.viewers.Add(1) == 1 {
		s.tasks.changed()
	}
	// This handler owns all session resources. Close first to unblock both
	// readers, join the workers, then release the viewer/task exactly once.
	defer func() {
		cancel()
		_ = ws.Close()
		if socket != nil {
			_ = socket.Close()
		}
		workers.Wait()
		if socket != nil {
			s.active.Add(-1)
		}
		if s.viewers.Add(-1) == 0 {
			s.tasks.changed()
		}
	}()
	var err error
	socket, err = s.dial(ctx)
	if err != nil {
		return
	}
	s.active.Add(1)
	outgoing := make(chan []byte, 2)
	errorsSeen := make(chan error, 3)
	pong := make(chan struct{}, 1)
	clientPing := make(chan string, 1)
	ws.SetPongHandler(func(string) error {
		select {
		case pong <- struct{}{}:
		default:
		}
		return nil
	})
	ws.SetPingHandler(func(payload string) error {
		select {
		case clientPing <- payload:
		case <-ctx.Done():
			return ctx.Err()
		}
		return nil
	})
	// Gorilla's defaults write control messages from the reader. The writer
	// below owns every write instead; a peer close ends the relay.
	ws.SetCloseHandler(func(int, string) error { return nil })
	workers.Add(3)
	go func() { defer workers.Done(); websocketToSocket(ctx, ws, socket, errorsSeen) }()
	go func() { defer workers.Done(); socketToWebsocket(ctx, socket, outgoing, errorsSeen) }()
	go func() {
		defer workers.Done()
		websocketWriter(ctx, ws, outgoing, pong, clientPing, s.config.PingInterval, s.pongTimeout, errorsSeen)
	}()
	select {
	case <-errorsSeen:
	case <-ctx.Done():
	}
}

func websocketToSocket(ctx context.Context, ws *websocket.Conn, socket net.Conn, result chan<- error) {
	for {
		kind, reader, err := ws.NextReader()
		if err != nil {
			result <- err
			return
		}
		if kind != websocket.BinaryMessage {
			result <- errors.New("text WebSocket messages are not valid RFB")
			return
		}
		if _, err := io.Copy(socket, reader); err != nil {
			result <- err
			return
		}
		select {
		case <-ctx.Done():
			result <- ctx.Err()
			return
		default:
		}
	}
}

func socketToWebsocket(ctx context.Context, socket net.Conn, outgoing chan<- []byte, result chan<- error) {
	for {
		buffer := make([]byte, 32*1024)
		n, err := socket.Read(buffer)
		if n > 0 {
			select {
			case outgoing <- buffer[:n]:
			case <-ctx.Done():
				result <- ctx.Err()
				return
			}
		}
		if err != nil {
			result <- err
			return
		}
	}
}

func websocketWriter(ctx context.Context, ws *websocket.Conn, outgoing <-chan []byte, pong <-chan struct{}, clientPing <-chan string, pingInterval, pongTimeout time.Duration, result chan<- error) {
	ping := time.NewTimer(pingInterval)
	defer ping.Stop()
	deadline := time.NewTimer(pongTimeout)
	deadline.Stop()
	defer deadline.Stop()
	var deadlineC <-chan time.Time
	missed := 0
	for {
		select {
		case data := <-outgoing:
			ws.SetWriteDeadline(time.Now().Add(10 * time.Second))
			if err := ws.WriteMessage(websocket.BinaryMessage, data); err != nil {
				result <- err
				return
			}
		case payload := <-clientPing:
			ws.SetWriteDeadline(time.Now().Add(10 * time.Second))
			if err := ws.WriteMessage(websocket.PongMessage, []byte(payload)); err != nil {
				result <- err
				return
			}
		case <-ping.C:
			ws.SetWriteDeadline(time.Now().Add(10 * time.Second))
			if err := ws.WriteMessage(websocket.PingMessage, nil); err != nil {
				result <- err
				return
			}
			deadline.Reset(pongTimeout)
			deadlineC = deadline.C
			ping.Reset(pingInterval)
		case <-pong:
			missed = 0
			deadline.Stop()
			deadlineC = nil
		case <-deadlineC:
			deadlineC = nil
			missed++
			if missed >= 2 {
				result <- errors.New("two WebSocket pong deadlines missed")
				return
			}
		case <-ctx.Done():
			result <- ctx.Err()
			return
		}
	}
}
