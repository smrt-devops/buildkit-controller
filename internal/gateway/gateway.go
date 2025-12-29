// Package gateway implements the pool gateway for routing connections to workers.
package gateway

import (
	"context"
	"crypto/tls"
	"crypto/x509"
	"errors"
	"fmt"
	"io"
	"net"
	"strings"
	"sync"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"

	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

var (
	// Metrics
	connectionsTotal = promauto.NewCounterVec(
		prometheus.CounterOpts{
			Name: "buildkit_gateway_connections_total",
			Help: "Total number of gateway connections",
		},
		[]string{"pool", "status"},
	)

	activeConnections = promauto.NewGaugeVec(
		prometheus.GaugeOpts{
			Name: "buildkit_gateway_active_connections",
			Help: "Number of active gateway connections",
		},
		[]string{"pool"},
	)

	connectionDuration = promauto.NewHistogramVec(
		prometheus.HistogramOpts{
			Name:    "buildkit_gateway_connection_duration_seconds",
			Help:    "Duration of gateway connections",
			Buckets: prometheus.ExponentialBuckets(1, 2, 10), // 1s to ~17m
		},
		[]string{"pool"},
	)
)

// WorkerLookup is a function that looks up a worker endpoint by allocation token.
type WorkerLookup func(ctx context.Context, token string) (workerEndpoint string, err error)

// Gateway handles incoming connections and routes them to workers.
type Gateway struct {
	poolName     string
	listenAddr   string
	tlsConfig    *tls.Config
	workerLookup WorkerLookup
	workerTLS    *tls.Config // mTLS config for connecting to workers
	logger       utils.Logger

	listener net.Listener
	mu       sync.RWMutex
	running  bool
}

// Config holds gateway configuration.
type Config struct {
	PoolName     string
	ListenAddr   string
	TLSConfig    *tls.Config
	WorkerTLS    *tls.Config // mTLS for worker connections
	WorkerLookup WorkerLookup
	Logger       utils.Logger
}

// New creates a new Gateway.
func New(cfg Config) *Gateway {
	return &Gateway{
		poolName:     cfg.PoolName,
		listenAddr:   cfg.ListenAddr,
		tlsConfig:    cfg.TLSConfig,
		workerTLS:    cfg.WorkerTLS,
		workerLookup: cfg.WorkerLookup,
		logger:       cfg.Logger,
	}
}

// Start starts the gateway listener.
func (g *Gateway) Start(ctx context.Context) error {
	var err error

	if g.tlsConfig != nil {
		g.listener, err = tls.Listen("tcp", g.listenAddr, g.tlsConfig)
	} else {
		g.listener, err = net.Listen("tcp", g.listenAddr)
	}
	if err != nil {
		return fmt.Errorf("failed to start listener: %w", err)
	}

	g.mu.Lock()
	g.running = true
	g.mu.Unlock()

	g.logger.Info("Gateway started", "pool", g.poolName, "addr", g.listenAddr)

	go func() {
		<-ctx.Done()
		g.Stop()
	}()

	for {
		conn, err := g.listener.Accept()
		if err != nil {
			g.mu.RLock()
			running := g.running
			g.mu.RUnlock()

			if !running {
				return nil // Normal shutdown
			}
			g.logger.Error(err, "Accept error")
			continue
		}

		go g.handleConnection(ctx, conn)
	}
}

// Stop stops the gateway.
func (g *Gateway) Stop() {
	g.mu.Lock()
	g.running = false
	g.mu.Unlock()

	if g.listener != nil {
		g.listener.Close()
	}
	g.logger.Info("Gateway stopped", "pool", g.poolName)
}

// handleConnection handles an incoming connection.
func (g *Gateway) handleConnection(ctx context.Context, conn net.Conn) {
	startTime := time.Now()
	defer func() {
		conn.Close()
		connectionDuration.WithLabelValues(g.poolName).Observe(time.Since(startTime).Seconds())
	}()

	activeConnections.WithLabelValues(g.poolName).Inc()
	defer activeConnections.WithLabelValues(g.poolName).Dec()

	tlsConn, token, ok := g.handleTLSConnection(conn)
	if !ok {
		return
	}

	workerEndpoint, err := g.workerLookup(ctx, token)
	if err != nil {
		g.logger.Error(err, "Worker lookup failed", "token", maskToken(token))
		connectionsTotal.WithLabelValues(g.poolName, "lookup_failed").Inc()
		return
	}

	dialAddress := strings.TrimPrefix(workerEndpoint, "tcp://")
	if g.workerTLS == nil {
		g.logger.Error(fmt.Errorf("worker TLS config is nil"), "Cannot connect to worker")
		connectionsTotal.WithLabelValues(g.poolName, "no_worker_tls").Inc()
		return
	}

	dialer := &tls.Dialer{
		NetDialer: &net.Dialer{Timeout: 10 * time.Second},
		Config:    g.workerTLS,
	}
	workerConn, err := dialer.DialContext(ctx, "tcp", dialAddress)
	if err != nil {
		g.logger.Error(err, "Failed to connect to worker with mTLS", "endpoint", workerEndpoint, "dial_address", dialAddress)
		connectionsTotal.WithLabelValues(g.poolName, "worker_connect_failed").Inc()
		return
	}
	defer workerConn.Close()

	connectionsTotal.WithLabelValues(g.poolName, "success").Inc()
	g.logger.V(1).Info("Routing connection to worker", "endpoint", workerEndpoint, "dial_address", dialAddress, "token", maskToken(token))

	g.proxy(tlsConn, workerConn)
}

func (g *Gateway) handleTLSConnection(conn net.Conn) (*tls.Conn, string, bool) {
	if g.tlsConfig == nil {
		return nil, "", true
	}

	tlsConn, ok := conn.(*tls.Conn)
	if !ok {
		g.logger.Info("Non-TLS connection on TLS listener")
		connectionsTotal.WithLabelValues(g.poolName, "non_tls").Inc()
		return nil, "", false
	}

	if err := tlsConn.Handshake(); err != nil {
		if !isExpectedCloseError(err) {
			g.logger.Info("TLS handshake failed", "error", err)
		}
		connectionsTotal.WithLabelValues(g.poolName, "handshake_failed").Inc()
		return nil, "", false
	}

	state := tlsConn.ConnectionState()
	if len(state.PeerCertificates) == 0 {
		g.logger.Info("No client certificate provided")
		connectionsTotal.WithLabelValues(g.poolName, "no_cert").Inc()
		return nil, "", false
	}

	token := extractTokenFromCert(state.PeerCertificates[0])
	if token == "" {
		g.logger.Info("No allocation token in certificate")
		connectionsTotal.WithLabelValues(g.poolName, "no_token").Inc()
		return nil, "", false
	}

	return tlsConn, token, true
}

func (g *Gateway) proxy(client net.Conn, worker net.Conn) {
	var wg sync.WaitGroup
	wg.Add(2)

	go func() {
		defer wg.Done()
		defer worker.Close()
		if _, err := io.Copy(worker, client); err != nil && !isExpectedCloseError(err) {
			g.logger.V(1).Info("Error copying client to worker", "error", err)
		}
	}()

	go func() {
		defer wg.Done()
		defer client.Close()
		if _, err := io.Copy(client, worker); err != nil && !isExpectedCloseError(err) {
			g.logger.V(1).Info("Error copying worker to client", "error", err)
		}
	}()

	wg.Wait()
}

func extractTokenFromCert(cert *x509.Certificate) string {
	if strings.HasPrefix(cert.Subject.CommonName, "alloc:") {
		return strings.TrimPrefix(cert.Subject.CommonName, "alloc:")
	}

	for _, uri := range cert.URIs {
		if uri.Scheme == "buildkit" && uri.Host == "allocation" {
			return uri.Path[1:]
		}
	}

	if cert.Subject.CommonName != "" {
		return cert.Subject.CommonName
	}

	return ""
}

func maskToken(token string) string {
	if len(token) <= 8 {
		return "***"
	}
	return token[:8] + "..."
}

func isExpectedCloseError(err error) bool {
	if err == nil {
		return false
	}
	errStr := err.Error()
	return strings.Contains(errStr, "use of closed network connection") ||
		strings.Contains(errStr, "connection reset by peer") ||
		errors.Is(err, io.EOF)
}
