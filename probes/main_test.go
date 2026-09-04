package main

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/coder/websocket"
)

func TestEchoAndOrigin(t *testing.T) {
	s := newServer("https://sprite.example", 0)
	httpServer := httptest.NewServer(s.routes())
	defer httpServer.Close()

	wsURL := "ws" + strings.TrimPrefix(httpServer.URL, "http") + "/echo"
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	conn, _, err := websocket.Dial(ctx, wsURL, &websocket.DialOptions{HTTPHeader: http.Header{"Origin": []string{"https://sprite.example"}}})
	if err != nil {
		t.Fatal(err)
	}
	defer conn.CloseNow()
	conn.SetReadLimit(2 * 1024 * 1024)

	cases := []struct {
		messageType websocket.MessageType
		payload     []byte
	}{
		{messageType: websocket.MessageText, payload: []byte("hello")},
		{messageType: websocket.MessageBinary, payload: make([]byte, 1024*1024)},
	}
	for i := range cases[1].payload {
		cases[1].payload[i] = byte(i % 251)
	}
	for _, test := range cases {
		if err := conn.Write(ctx, test.messageType, test.payload); err != nil {
			t.Fatal(err)
		}
		messageType, echoed, err := conn.Read(ctx)
		if err != nil {
			t.Fatal(err)
		}
		if messageType != test.messageType || len(echoed) != len(test.payload) || !bytes.Equal(echoed, test.payload) {
			t.Fatalf("echo type/length = (%v, %d), want (%v, %d)", messageType, len(echoed), test.messageType, len(test.payload))
		}
	}

	for _, origins := range [][]string{
		nil, {"null"}, {"malformed"}, {"https://other.example"},
		{"http://sprite.example"}, {"https://sprite.example:443"},
		{"https://sprite.example, https://other.example"},
		{"https://sprite.example", "https://other.example"},
	} {
		unexpected, response, err := websocket.Dial(ctx, wsURL, &websocket.DialOptions{HTTPHeader: http.Header{"Origin": origins}})
		if err == nil {
			unexpected.CloseNow()
			t.Fatalf("Origin %q was accepted", origins)
		}
		if response == nil || response.StatusCode != http.StatusForbidden {
			t.Fatalf("Origin %q: status = %v, want 403", origins, response)
		}
	}
}

func TestHealth(t *testing.T) {
	s := newServer("https://sprite.example", 0)
	recorder := httptest.NewRecorder()
	s.healthz(recorder, httptest.NewRequest(http.MethodGet, "/healthz", nil))
	if recorder.Code != http.StatusOK {
		t.Fatalf("status = %d", recorder.Code)
	}
	var got health
	if err := json.Unmarshal(recorder.Body.Bytes(), &got); err != nil {
		t.Fatal(err)
	}
	if got.StartedAt.IsZero() || got.ConnectionsActive != 0 {
		t.Fatalf("unexpected health: %+v", got)
	}
}
