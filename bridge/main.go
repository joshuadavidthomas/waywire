package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"
)

const (
	defaultConfigPath  = "/etc/sprite-desktop/config.json"
	defaultVersionPath = "/etc/sprite-desktop/version.json"
	defaultTaskSocket  = "/.sprite/api.sock"
)

func main() {
	if err := run(os.Args[1:]); err != nil {
		log.Fatal(err)
	}
}

func run(args []string) error {
	flags := flag.NewFlagSet("sprite-desktop-bridge", flag.ContinueOnError)
	configPath := flags.String("config", defaultConfigPath, "path to config.json")
	checkConfig := flags.Bool("check-config", false, "validate configuration and exit")
	showVersion := flags.Bool("version", false, "print binary release and source revision")
	previousVersion := flags.String("check-upgrade-from", "", "validate upgrade from a prior release and exit")
	if err := flags.Parse(args); err != nil {
		return err
	}
	if flags.NArg() != 0 {
		return fmt.Errorf("unexpected positional arguments")
	}
	if *showVersion {
		fmt.Printf("%s %s\n", buildVersion, buildSource)
		return nil
	}
	if *previousVersion != "" {
		return checkUpgrade(*previousVersion, buildVersion)
	}
	cfg, err := loadConfig(*configPath)
	if err != nil {
		return fmt.Errorf("config: %w", err)
	}
	if *checkConfig {
		return nil
	}
	version, err := loadVersion(defaultVersionPath)
	if err != nil {
		return fmt.Errorf("version metadata: %w", err)
	}
	dialer := &net.Dialer{Timeout: 3 * time.Second}
	dial := func(ctx context.Context) (net.Conn, error) {
		return dialer.DialContext(ctx, "unix", cfg.SocketPath)
	}
	bridge := newBridgeServer(cfg, version, dial, defaultTaskSocket)
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	tasksCtx, stopTasks := context.WithCancel(context.Background())
	tasksDone := make(chan struct{})
	go func() { defer close(tasksDone); bridge.tasks.run(tasksCtx) }()

	httpServer := &http.Server{
		Addr:              ":8080",
		BaseContext:       func(net.Listener) context.Context { return ctx },
		Handler:           bridge.handler(),
		ReadHeaderTimeout: 10 * time.Second,
		IdleTimeout:       60 * time.Second,
	}
	log.Printf("sprite desktop bridge %s (%s) listening on :8080", buildVersion, buildSource)
	serverDone := make(chan error, 1)
	go func() { serverDone <- httpServer.ListenAndServe() }()
	var serveError error
	select {
	case serveError = <-serverDone:
		stop()
	case <-ctx.Done():
	}
	shutdown, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()
	if err := httpServer.Shutdown(shutdown); err != nil {
		_ = httpServer.Close()
	}
	bridge.sessions.Wait()
	stopTasks()
	<-tasksDone
	if serveError != nil && serveError != http.ErrServerClosed {
		return serveError
	}
	return nil
}
