package main

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"net/url"
	"os"
	"time"
)

const defaultSocketPath = "/tmp/sprite-desktop/rfb.sock"

type configFile struct {
	Origin       string          `json:"origin"`
	SocketPath   string          `json:"socket_path"`
	PingInterval string          `json:"ping_interval"`
	Keepalive    keepaliveConfig `json:"keepalive"`
}

type keepaliveConfig struct {
	Task bool `json:"task"`
}

type config struct {
	Origin       string
	SocketPath   string
	PingInterval time.Duration
	TaskHold     bool
}

func loadConfig(path string) (config, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return config{}, err
	}
	var raw configFile
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	if err := dec.Decode(&raw); err != nil {
		return config{}, fmt.Errorf("decode config: %w", err)
	}
	if err := dec.Decode(&struct{}{}); err != io.EOF {
		return config{}, fmt.Errorf("decode config: trailing JSON value")
	}
	if raw.SocketPath == "" {
		raw.SocketPath = defaultSocketPath
	}
	if raw.PingInterval == "" {
		raw.PingInterval = "20s"
	}
	origin, err := url.Parse(raw.Origin)
	if err != nil || origin.Scheme != "https" || origin.Hostname() == "" || origin.Port() != "" || origin.User != nil || origin.Path != "" || origin.RawPath != "" || origin.RawQuery != "" || origin.ForceQuery || origin.Fragment != "" || origin.Opaque != "" {
		return config{}, fmt.Errorf("origin must be a canonical HTTPS origin without a path")
	}
	if origin.String() != raw.Origin {
		return config{}, fmt.Errorf("origin is not canonical")
	}
	if raw.SocketPath[0] != '/' {
		return config{}, fmt.Errorf("socket_path must be absolute")
	}
	interval, err := time.ParseDuration(raw.PingInterval)
	if err != nil || interval != 20*time.Second {
		return config{}, fmt.Errorf("ping_interval must be 20s")
	}
	return config{Origin: raw.Origin, SocketPath: raw.SocketPath, PingInterval: interval, TaskHold: raw.Keepalive.Task}, nil
}
