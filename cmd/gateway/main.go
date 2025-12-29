/*
Gateway is the BuildKit pool gateway that handles TLS termination and routing.

The gateway:
1. Terminates TLS connections from clients
2. Validates allocation tokens from client certificates
3. Routes connections to the appropriate worker via mTLS
*/
package main

import (
	"bytes"
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/prometheus/client_golang/prometheus/promhttp"

	"github.com/smrt-devops/buildkit-controller/internal/gateway"
	"github.com/smrt-devops/buildkit-controller/internal/utils"
)

func main() {
	var (
		poolName           = flag.String("pool-name", "", "Pool name")
		poolNamespace      = flag.String("pool-namespace", "default", "Pool namespace")
		listenAddr         = flag.String("listen-addr", "0.0.0.0:1235", "Gateway listen address")
		metricsAddr        = flag.String("metrics-addr", "0.0.0.0:9090", "Metrics server address")
		controllerEndpoint = flag.String("controller-endpoint", "http://buildkit-controller.buildkit-system.svc:8082", "Controller API endpoint")
		serverCertPath     = flag.String("server-cert", "/etc/gateway/tls/tls.crt", "Server certificate path")
		serverKeyPath      = flag.String("server-key", "/etc/gateway/tls/tls.key", "Server key path")
		caCertPath         = flag.String("ca-cert", "/etc/gateway/tls/ca.crt", "CA certificate path")
	)
	flag.Parse()

	log := utils.NewLoggerFromEnv()

	if *poolName == "" {
		log.Error(nil, "pool-name is required")
		os.Exit(1)
	}

	log.Info("Starting BuildKit gateway",
		"pool", *poolName,
		"namespace", *poolNamespace,
		"listenAddr", *listenAddr,
		"controllerEndpoint", *controllerEndpoint,
	)

	// Load server TLS config
	serverTLS, err := loadServerTLS(*serverCertPath, *serverKeyPath, *caCertPath)
	if err != nil {
		log.Error(err, "Failed to load server TLS config")
		os.Exit(1)
	}

	workerTLS, err := loadWorkerTLS()
	if err != nil {
		log.Error(err, "Failed to load worker TLS config")
		os.Exit(1)
	}
	if workerTLS == nil {
		log.Error(nil, "Worker TLS config is nil")
		os.Exit(1)
	}

	// Create worker lookup function that calls controller API
	workerLookup := createWorkerLookup(*controllerEndpoint)

	// Create gateway
	gw := gateway.New(gateway.Config{
		PoolName:     *poolName,
		ListenAddr:   *listenAddr,
		TLSConfig:    serverTLS,
		WorkerTLS:    workerTLS,
		WorkerLookup: workerLookup,
		Logger:       log,
	})

	// Handle shutdown
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	// Start metrics server
	metricsServer := &http.Server{
		Addr: *metricsAddr,
	}
	mux := http.NewServeMux()
	mux.Handle("/metrics", promhttp.Handler())
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, r *http.Request) {
		w.WriteHeader(http.StatusOK)
	})
	metricsServer.Handler = mux

	go func() {
		log.Info("Starting metrics server", "addr", *metricsAddr)
		if err := metricsServer.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Error(err, "Metrics server failed")
		}
	}()

	go func() {
		<-ctx.Done()
		shutdownCtx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		if err := metricsServer.Shutdown(shutdownCtx); err != nil {
			log.Error(err, "Error shutting down metrics server")
		}
	}()

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	go func() {
		sig := <-sigCh
		log.Info("Received signal, shutting down", "signal", sig)
		cancel()
	}()

	// Start gateway
	if err := gw.Start(ctx); err != nil {
		log.Error(err, "Gateway failed")
		os.Exit(1)
	}

	log.Info("Gateway stopped")
}

func loadServerTLS(certPath, keyPath, caPath string) (*tls.Config, error) {
	cert, err := tls.LoadX509KeyPair(certPath, keyPath)
	if err != nil {
		return nil, fmt.Errorf("failed to load server cert: %w", err)
	}

	caCert, err := os.ReadFile(caPath)
	if err != nil {
		return nil, fmt.Errorf("failed to load CA cert: %w", err)
	}

	caPool := x509.NewCertPool()
	if !caPool.AppendCertsFromPEM(caCert) {
		return nil, fmt.Errorf("failed to parse CA cert")
	}

	return &tls.Config{
		Certificates: []tls.Certificate{cert},
		ClientAuth:   tls.RequireAndVerifyClientCert,
		ClientCAs:    caPool,
		MinVersion:   tls.VersionTLS12,
	}, nil
}

func loadWorkerTLS() (*tls.Config, error) {
	const (
		workerCertPath   = "/etc/gateway/worker-tls/client.crt"
		workerKeyPath    = "/etc/gateway/worker-tls/client.key"
		workerCACertPath = "/etc/gateway/worker-tls/ca.crt"
	)

	if _, err := os.Stat(workerCertPath); os.IsNotExist(err) {
		return nil, fmt.Errorf("worker client certificate not found at %s", workerCertPath)
	}
	if _, err := os.Stat(workerKeyPath); os.IsNotExist(err) {
		return nil, fmt.Errorf("worker client key not found at %s", workerKeyPath)
	}
	if _, err := os.Stat(workerCACertPath); os.IsNotExist(err) {
		return nil, fmt.Errorf("worker CA certificate not found at %s", workerCACertPath)
	}

	// Load client certificate and key
	cert, err := tls.LoadX509KeyPair(workerCertPath, workerKeyPath)
	if err != nil {
		return nil, fmt.Errorf("failed to load worker client cert: %w", err)
	}

	// Load CA certificate
	caCert, err := os.ReadFile(workerCACertPath)
	if err != nil {
		return nil, fmt.Errorf("failed to load worker CA cert: %w", err)
	}

	caPool := x509.NewCertPool()
	if !caPool.AppendCertsFromPEM(caCert) {
		return nil, fmt.Errorf("failed to parse worker CA cert")
	}

	return &tls.Config{
		Certificates:       []tls.Certificate{cert},
		RootCAs:            caPool,
		MinVersion:         tls.VersionTLS12,
		InsecureSkipVerify: true,
		VerifyPeerCertificate: func(rawCerts [][]byte, verifiedChains [][]*x509.Certificate) error {
			if len(rawCerts) == 0 {
				return fmt.Errorf("no certificate provided")
			}
			workerCert, err := x509.ParseCertificate(rawCerts[0])
			if err != nil {
				return fmt.Errorf("failed to parse certificate: %w", err)
			}
			opts := x509.VerifyOptions{
				Roots: caPool,
			}
			if _, err := workerCert.Verify(opts); err != nil {
				return fmt.Errorf("certificate not signed by trusted CA: %w", err)
			}
			return nil
		},
	}, nil
}

func createWorkerLookup(controllerEndpoint string) gateway.WorkerLookup {
	client := &http.Client{
		Timeout: 10 * time.Second,
	}

	return func(ctx context.Context, token string) (string, error) {
		url := fmt.Sprintf("%s/api/v1/workers/lookup", controllerEndpoint)

		reqBody, err := json.Marshal(map[string]string{"token": token})
		if err != nil {
			return "", fmt.Errorf("failed to marshal request: %w", err)
		}

		req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(reqBody))
		if err != nil {
			return "", fmt.Errorf("failed to create request: %w", err)
		}
		req.Header.Set("Content-Type", "application/json")

		resp, err := client.Do(req)
		if err != nil {
			return "", fmt.Errorf("failed to lookup worker: %w", err)
		}
		defer resp.Body.Close()

		if resp.StatusCode != http.StatusOK {
			body, _ := io.ReadAll(resp.Body)
			return "", fmt.Errorf("worker lookup failed: status %d, body: %s", resp.StatusCode, string(body))
		}

		var result struct {
			WorkerEndpoint string `json:"workerEndpoint"`
		}
		if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
			return "", fmt.Errorf("failed to decode response: %w", err)
		}

		if result.WorkerEndpoint == "" {
			return "", fmt.Errorf("empty worker endpoint")
		}

		return result.WorkerEndpoint, nil
	}
}
