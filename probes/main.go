package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"log"
	"net"
	"net/http"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	"github.com/coder/websocket"
)

const page = `<!doctype html>
<html lang="en">
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sprite Desktop M0 probe</title>
<h1>Sprite Desktop M0 probe</h1>
<pre id="status">connecting</pre>
<script>
const status = document.querySelector('#status');
const expectedText = 'sprite-desktop-m0';
const expectedSize = 1024 * 1024;
const lines = [];
function report(line) { lines.push(new Date().toISOString() + ' ' + line); status.textContent = lines.join('\n'); }
const scheme = location.protocol === 'https:' ? 'wss:' : 'ws:';
const ws = new WebSocket(scheme + '//' + location.host + '/echo');
ws.binaryType = 'arraybuffer';
const payload = Uint8Array.from({length: expectedSize}, (_, i) => i % 251);
ws.onopen = () => { report('connected'); ws.send(expectedText); ws.send(payload); };
ws.onmessage = event => {
  if (typeof event.data === 'string') report(event.data === expectedText ? 'text echo ok' : 'text echo mismatch');
  else report(event.data.byteLength === expectedSize && new Uint8Array(event.data).every((value, i) => value === payload[i]) ? '1 MiB binary echo ok' : 'binary echo mismatch');
};
ws.onerror = () => report('websocket error');
ws.onclose = event => report('closed: ' + event.code + (event.reason ? ' ' + event.reason : ''));
</script>
`

type metrics struct {
	startedAt      time.Time
	active         atomic.Int64
	total          atomic.Uint64
	textMessages   atomic.Uint64
	binaryMessages atomic.Uint64
	textBytes      atomic.Uint64
	binaryBytes    atomic.Uint64
	lastUnixNano   atomic.Int64
	mu             sync.RWMutex
	lastOrigin     string
	lastHost       string
	lastOriginOK   bool
}

type server struct {
	origin       string
	pingInterval time.Duration
	metrics      *metrics
	taskUpdates  chan struct{}
	taskHeld     atomic.Bool
}

type health struct {
	TaskHeld           bool       `json:"task_held"`
	StartedAt          time.Time  `json:"started_at"`
	ConnectionsActive  int64      `json:"connections_active"`
	ConnectionsTotal   uint64     `json:"connections_total"`
	TextEchoes         uint64     `json:"text_echoes"`
	BinaryEchoes       uint64     `json:"binary_echoes"`
	TextBytes          uint64     `json:"text_bytes"`
	BinaryBytes        uint64     `json:"binary_bytes"`
	LastActivity       *time.Time `json:"last_activity"`
	LastOrigin         string     `json:"last_origin,omitempty"`
	LastOriginAccepted bool       `json:"last_origin_accepted"`
	LastHost           string     `json:"last_host,omitempty"`
}

func newServer(origin string, pingInterval time.Duration) *server {
	return &server{origin: origin, pingInterval: pingInterval, metrics: &metrics{startedAt: time.Now().UTC()}}
}

func (s *server) routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("/", s.index)
	mux.HandleFunc("/echo", s.echo)
	mux.HandleFunc("/healthz", s.healthz)
	return mux
}

func (s *server) index(w http.ResponseWriter, r *http.Request) {
	if r.URL.Path != "/" {
		http.NotFound(w, r)
		return
	}
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		w.Header().Set("Allow", "GET, HEAD")
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	_, _ = w.Write([]byte(page))
}

func (s *server) echo(w http.ResponseWriter, r *http.Request) {
	origins := r.Header.Values("Origin")
	origin := r.Header.Get("Origin")
	accepted := len(origins) == 1 && origin == s.origin
	s.metrics.mu.Lock()
	s.metrics.lastOrigin = origin
	s.metrics.lastHost = r.Host
	s.metrics.lastOriginOK = accepted
	s.metrics.mu.Unlock()
	s.touch()

	if r.Method != http.MethodGet {
		w.Header().Set("Allow", "GET")
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	if !accepted {
		http.Error(w, "forbidden origin", http.StatusForbidden)
		return
	}

	conn, err := websocket.Accept(w, r, &websocket.AcceptOptions{OriginPatterns: []string{s.origin}})
	if err != nil {
		log.Printf("websocket accept: %v", err)
		return
	}
	conn.SetReadLimit(2 * 1024 * 1024)
	s.metrics.active.Add(1)
	s.metrics.total.Add(1)
	s.notifyTask()
	defer func() {
		s.metrics.active.Add(-1)
		s.notifyTask()
		_ = conn.Close(websocket.StatusNormalClosure, "probe closed")
		s.touch()
	}()

	ctx, cancel := context.WithCancel(r.Context())
	defer cancel()
	if s.pingInterval > 0 {
		go s.ping(ctx, conn)
	}
	for {
		messageType, data, err := conn.Read(ctx)
		if err != nil {
			if websocket.CloseStatus(err) == -1 && !errors.Is(err, context.Canceled) {
				log.Printf("websocket read: %v", err)
			}
			return
		}
		s.touch()
		s.count(messageType, len(data))
		if err := conn.Write(ctx, messageType, data); err != nil {
			log.Printf("websocket write: %v", err)
			return
		}
	}
}

func (s *server) ping(ctx context.Context, conn *websocket.Conn) {
	ticker := time.NewTicker(s.pingInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			pingCtx, cancel := context.WithTimeout(ctx, 15*time.Second)
			err := conn.Ping(pingCtx)
			cancel()
			if err != nil {
				_ = conn.CloseNow()
				return
			}
			s.touch()
		}
	}
}

func (s *server) count(messageType websocket.MessageType, size int) {
	switch messageType {
	case websocket.MessageText:
		s.metrics.textMessages.Add(1)
		s.metrics.textBytes.Add(uint64(size))
	case websocket.MessageBinary:
		s.metrics.binaryMessages.Add(1)
		s.metrics.binaryBytes.Add(uint64(size))
	}
}

func (s *server) touch() { s.metrics.lastUnixNano.Store(time.Now().UnixNano()) }

func (s *server) healthz(w http.ResponseWriter, r *http.Request) {
	if r.Method != http.MethodGet && r.Method != http.MethodHead {
		w.Header().Set("Allow", "GET, HEAD")
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	s.metrics.mu.RLock()
	h := health{
		TaskHeld:           s.taskHeld.Load(),
		StartedAt:          s.metrics.startedAt,
		ConnectionsActive:  s.metrics.active.Load(),
		ConnectionsTotal:   s.metrics.total.Load(),
		TextEchoes:         s.metrics.textMessages.Load(),
		BinaryEchoes:       s.metrics.binaryMessages.Load(),
		TextBytes:          s.metrics.textBytes.Load(),
		BinaryBytes:        s.metrics.binaryBytes.Load(),
		LastOrigin:         s.metrics.lastOrigin,
		LastOriginAccepted: s.metrics.lastOriginOK,
		LastHost:           s.metrics.lastHost,
	}
	s.metrics.mu.RUnlock()
	if unixNano := s.metrics.lastUnixNano.Load(); unixNano != 0 {
		last := time.Unix(0, unixNano).UTC()
		h.LastActivity = &last
	}
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("Content-Type", "application/json")
	if r.Method == http.MethodHead {
		return
	}
	if err := json.NewEncoder(w).Encode(h); err != nil {
		log.Printf("health response: %v", err)
	}
}

// Only the M0 task-held trial enables this loop. HTTP requests are serialized;
// queued notifications read the latest count rather than carrying stale counts.
func (s *server) notifyTask() {
	select {
	case s.taskUpdates <- struct{}{}:
	default:
	}
}

func (s *server) tasks(ctx context.Context) {
	client := &http.Client{
		Timeout: 5 * time.Second,
		Transport: &http.Transport{DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
			return (&net.Dialer{}).DialContext(ctx, "unix", "/.sprite/api.sock")
		}},
	}
	defer client.CloseIdleConnections()
	ticker := time.NewTicker(30 * time.Second)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			return
		case <-s.taskUpdates:
		case <-ticker.C:
		}
		if s.metrics.active.Load() == 0 && !s.taskHeld.Load() {
			continue
		}
		method, body := http.MethodDelete, ""
		if s.metrics.active.Load() > 0 {
			method, body = http.MethodPut, `{"expire":"90s"}`
		}
		req, err := http.NewRequestWithContext(ctx, method, "http://sprite/v1/tasks/desktop-m0-hold", strings.NewReader(body))
		if err != nil {
			log.Printf("task request: %v", err)
			continue
		}
		req.Header.Set("Content-Type", "application/json")
		response, err := client.Do(req)
		if err != nil {
			log.Printf("task operation: %v", err)
			continue
		}
		response.Body.Close()
		if response.StatusCode == 200 || response.StatusCode == 204 || (method == http.MethodDelete && response.StatusCode == 404) {
			s.taskHeld.Store(method == http.MethodPut)
		} else {
			log.Printf("task operation returned HTTP %d", response.StatusCode)
		}
	}
}

func main() {
	origin := flag.String("origin", "", "exact HTTPS Origin accepted for /echo (required)")
	listen := flag.String("listen", ":8080", "HTTP listen address")
	pingInterval := flag.Duration("ping-interval", 0, "WebSocket ping interval; use 20s for the M0 ping trial")
	taskHold := flag.Bool("task-hold", false, "hold a 90s Sprite task while echo clients are attached (M0 trial only)")
	flag.Parse()
	if *origin == "" {
		log.Fatal("-origin is required")
	}
	if *pingInterval < 0 {
		log.Fatal("-ping-interval must not be negative")
	}

	s := newServer(*origin, *pingInterval)
	if *taskHold {
		s.taskUpdates = make(chan struct{}, 1)
		go s.tasks(context.Background())
	}
	log.Printf("M0 probe listening on %s; accepted origin %s; ping interval %s", *listen, *origin, *pingInterval)
	if err := http.ListenAndServe(*listen, s.routes()); err != nil {
		log.Fatal(err)
	}
}
