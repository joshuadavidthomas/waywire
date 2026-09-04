package main

import (
	"bytes"
	"context"
	"io"
	"log"
	"net"
	"net/http"
	"sync/atomic"
	"time"
)

const taskURL = "http://sprite/v1/tasks/sprite-desktop-viewer"

type taskManager struct {
	enabled bool
	// A failed PUT may have reached the API before its response was lost.
	// Only a successful DELETE clears this obligation.
	needsRelease bool
	viewers      *atomic.Int64
	held         atomic.Bool
	wake         chan struct{}
	client       *http.Client
}

func newTaskManager(enabled bool, viewers *atomic.Int64, socket string) *taskManager {
	transport := &http.Transport{DialContext: func(ctx context.Context, _, _ string) (net.Conn, error) {
		return (&net.Dialer{}).DialContext(ctx, "unix", socket)
	}}
	return &taskManager{
		enabled: enabled,
		viewers: viewers,
		wake:    make(chan struct{}, 1),
		client:  &http.Client{Transport: transport, Timeout: 5 * time.Second},
	}
}

func (m *taskManager) changed() {
	if !m.enabled {
		return
	}
	select {
	case m.wake <- struct{}{}:
	default:
	}
}

func (m *taskManager) run(ctx context.Context) {
	if !m.enabled {
		return
	}
	defer m.client.CloseIdleConnections()
	ticker := time.NewTicker(30 * time.Second)
	defer ticker.Stop()
	for {
		select {
		case <-ctx.Done():
			if m.viewers.Load() == 0 {
				m.delete(context.Background())
			}
			return
		case <-m.wake:
			m.sync(ctx)
		case <-ticker.C:
			m.sync(ctx)
		}
	}
}

func (m *taskManager) sync(ctx context.Context) {
	if m.viewers.Load() > 0 {
		m.put(ctx)
	} else if m.needsRelease {
		m.delete(ctx)
	}
}

func (m *taskManager) put(ctx context.Context) {
	req, err := http.NewRequestWithContext(ctx, http.MethodPut, taskURL, bytes.NewBufferString(`{"expire":"90s"}`))
	if err != nil {
		m.held.Store(false)
		return
	}
	req.Header.Set("Content-Type", "application/json")
	m.needsRelease = true
	resp, err := m.client.Do(req)
	if err != nil {
		log.Printf("renew viewer task: %v", err)
		m.held.Store(false)
		return
	}
	io.Copy(io.Discard, resp.Body)
	resp.Body.Close()
	m.held.Store(resp.StatusCode >= 200 && resp.StatusCode < 300)
	if !m.held.Load() {
		log.Printf("renew viewer task: HTTP %d", resp.StatusCode)
	}
}

func (m *taskManager) delete(ctx context.Context) {
	req, err := http.NewRequestWithContext(ctx, http.MethodDelete, taskURL, nil)
	if err != nil {
		return
	}
	resp, err := m.client.Do(req)
	if err != nil {
		log.Printf("release viewer task: %v", err)
		return
	}
	io.Copy(io.Discard, resp.Body)
	resp.Body.Close()
	if (resp.StatusCode >= 200 && resp.StatusCode < 300) || resp.StatusCode == http.StatusNotFound {
		m.needsRelease = false
		m.held.Store(false)
	} else {
		log.Printf("release viewer task: HTTP %d", resp.StatusCode)
	}
}
