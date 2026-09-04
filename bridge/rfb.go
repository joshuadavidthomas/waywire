package main

import (
	"context"
	"encoding/binary"
	"fmt"
	"io"
	"net"
	"sync"
	"sync/atomic"
	"time"
)

type socketDialer func(context.Context) (net.Conn, error)

type rfbReadiness struct {
	dial    socketDialer
	viewers *atomic.Int64
	active  *atomic.Int64

	mu        sync.Mutex
	checked   time.Time
	ready     bool
	probeDone chan struct{}
}

func (r *rfbReadiness) status(ctx context.Context) bool {
	if r.active.Load() > 0 {
		return true
	}
	r.mu.Lock()
	if r.viewers.Load() > 0 || time.Since(r.checked) < 10*time.Second {
		ready := r.ready
		r.mu.Unlock()
		return r.active.Load() > 0 || ready
	}
	if r.probeDone != nil {
		done := r.probeDone
		r.mu.Unlock()
		select {
		case <-done:
			return r.status(ctx)
		case <-ctx.Done():
			return false
		}
	}
	r.probeDone = make(chan struct{})
	done := r.probeDone
	r.mu.Unlock()

	// Finish an in-flight shared handshake even if a viewer arrives. Aborting
	// it mid-negotiation can count as a failed login in TigerVNC.
	err := probeRFB(context.Background(), r.dial)
	r.mu.Lock()
	if r.viewers.Load() == 0 {
		r.ready = err == nil
		r.checked = time.Now()
	}
	r.probeDone = nil
	close(done)
	ready := r.ready
	r.mu.Unlock()
	return r.active.Load() > 0 || ready
}

func probeRFB(parent context.Context, dial socketDialer) error {
	ctx, cancel := context.WithTimeout(parent, 3*time.Second)
	defer cancel()
	conn, err := dial(ctx)
	if err != nil {
		return err
	}
	defer conn.Close()
	stopClose := context.AfterFunc(ctx, func() { conn.Close() })
	defer stopClose()
	if deadline, ok := ctx.Deadline(); ok {
		conn.SetDeadline(deadline)
	}

	version := make([]byte, 12)
	if _, err := io.ReadFull(conn, version); err != nil {
		return err
	}
	if string(version) != "RFB 003.008\n" {
		return fmt.Errorf("unsupported RFB version %q", version)
	}
	if _, err := conn.Write(version); err != nil {
		return err
	}
	var count [1]byte
	if _, err := io.ReadFull(conn, count[:]); err != nil {
		return err
	}
	if count[0] == 0 {
		return fmt.Errorf("RFB server offered no security types")
	}
	security := make([]byte, int(count[0]))
	if _, err := io.ReadFull(conn, security); err != nil {
		return err
	}
	foundNone := false
	for _, kind := range security {
		foundNone = foundNone || kind == 1
	}
	if !foundNone {
		return fmt.Errorf("RFB server did not offer None security")
	}
	if _, err := conn.Write([]byte{1}); err != nil {
		return err
	}
	var result [4]byte
	if _, err := io.ReadFull(conn, result[:]); err != nil {
		return err
	}
	if binary.BigEndian.Uint32(result[:]) != 0 {
		return fmt.Errorf("RFB security negotiation failed")
	}
	if _, err := conn.Write([]byte{1}); err != nil {
		return err
	}
	header := make([]byte, 24)
	if _, err := io.ReadFull(conn, header); err != nil {
		return err
	}
	nameLength := binary.BigEndian.Uint32(header[20:24])
	if nameLength > 1<<20 {
		return fmt.Errorf("RFB desktop name is too large")
	}
	if _, err := io.CopyN(io.Discard, conn, int64(nameLength)); err != nil {
		return err
	}
	return nil
}
