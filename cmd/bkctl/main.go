/*
bkctl is the BuildKit Controller CLI.

It wraps docker buildx commands with automatic pool allocation and certificate management.

Usage:

	bkctl build --pool <pool-name> [docker buildx build args...]
	bkctl allocate --pool <pool-name>
	bkctl release --token <token>
	bkctl status --pool <pool-name>
*/
package main

import (
	"bytes"
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"
)

const (
	defaultControllerEndpoint = "http://localhost:8082"
	defaultNamespace          = "buildkit-system"
)

// oidcConfig holds OIDC token generation configuration
type oidcConfig struct {
	actor      string
	repository string
	subject    string
	audience   string
	issuer     string
}

func main() {
	if len(os.Args) < 2 {
		printUsage()
		os.Exit(1)
	}

	cmd := os.Args[1]

	switch cmd {
	case "build":
		runBuild(os.Args[2:])
	case "allocate":
		runAllocate(os.Args[2:])
	case "release":
		runRelease(os.Args[2:])
	case "status":
		runStatus(os.Args[2:])
	case "oidc-token":
		runOIDCToken(os.Args[2:])
	case "version":
		fmt.Println("bkctl v0.1.0")
	case "help", "--help", "-h":
		printUsage()
	default:
		fmt.Fprintf(os.Stderr, "Unknown command: %s\n", cmd)
		printUsage()
		os.Exit(1)
	}
}

func printUsage() {
	fmt.Println(`bkctl - BuildKit Controller CLI

Usage:
  bkctl build --pool <pool-name> [--namespace <ns>] [--oidc-actor <actor>] [--oidc-repository <repo>] [--] [docker buildx build args...]
  bkctl allocate --pool <pool-name> [--namespace <ns>] [--ttl <duration>] [--oidc-actor <actor>] [--oidc-repository <repo>]
  bkctl release --token <token> [--oidc-actor <actor>] [--oidc-repository <repo>]
  bkctl status --pool <pool-name> [--namespace <ns>] [--oidc-actor <actor>] [--oidc-repository <repo>]
  bkctl oidc-token [--issuer <url>] [--actor <name>] [--repository <repo>]

Environment Variables:
  BKCTL_ENDPOINT        Controller API endpoint (default: http://localhost:8082)
  BKCTL_TOKEN           Bearer token for authentication (OIDC or ServiceAccount)
                        If not set, bkctl will auto-generate OIDC token if mock-oidc is available
  BKCTL_NAMESPACE       Default namespace (default: buildkit-system)
  BKCTL_TLS_SKIP_VERIFY Skip TLS certificate verification (default: false)
                        Set to "true" to skip verification (useful for self-signed certs in dev)
  BKCTL_TLS_CA_CERT     Path to CA certificate file for TLS verification
                        If set, this CA will be used instead of system CAs

Examples:
  # Build an image using a pool (auto-generates OIDC token if BKCTL_TOKEN not set)
  bkctl build --pool prod-pool -- -t myimage:latest .

  # Allocate a worker with specific OIDC identity
  bkctl allocate --pool prod-pool --oidc-actor my-user --oidc-repository my-org/my-repo

  # Check pool status (auto-generates OIDC token)
  bkctl status --pool prod-pool

  # Explicitly set token (overrides auto-generation)
  export BKCTL_TOKEN=$(bkctl oidc-token --actor my-user --repository my-org/my-repo)
  bkctl allocate --pool prod-pool`)
}

type allocateResponse struct {
	WorkerName      string `json:"workerName"`
	Token           string `json:"token"`
	Endpoint        string `json:"endpoint"`
	GatewayEndpoint string `json:"gatewayEndpoint"`
	ExpiresAt       string `json:"expiresAt"`
	CACert          string `json:"caCert"`
	ClientCert      string `json:"clientCert"`
	ClientKey       string `json:"clientKey"`
}

func runBuild(args []string) {
	poolName := ""
	namespace := getEnvOrDefault("BKCTL_NAMESPACE", defaultNamespace)
	ttl := "1h"
	var oidcCfg oidcConfig

	// Parse bkctl args until we hit --
	var dockerArgs []string
	inDockerArgs := false

	for i := 0; i < len(args); i++ {
		if args[i] == "--" {
			inDockerArgs = true
			continue
		}

		if inDockerArgs {
			dockerArgs = append(dockerArgs, args[i])
			continue
		}

		switch args[i] {
		case "--pool", "-p":
			if i+1 < len(args) {
				poolName = args[i+1]
				i++
			}
		case "--namespace", "-n":
			if i+1 < len(args) {
				namespace = args[i+1]
				i++
			}
		case "--ttl":
			if i+1 < len(args) {
				ttl = args[i+1]
				i++
			}
		case "--oidc-actor":
			if i+1 < len(args) {
				oidcCfg.actor = args[i+1]
				i++
			}
		case "--oidc-repository":
			if i+1 < len(args) {
				oidcCfg.repository = args[i+1]
				i++
			}
		case "--oidc-subject":
			if i+1 < len(args) {
				oidcCfg.subject = args[i+1]
				i++
			}
		default:
			// Pass through to docker
			dockerArgs = append(dockerArgs, args[i])
		}
	}

	if poolName == "" {
		fmt.Fprintln(os.Stderr, "Error: --pool is required")
		os.Exit(1)
	}

	// Allocate a worker
	fmt.Printf("🔄 Allocating worker from pool '%s'...\n", poolName)
	resp, err := allocateWorker(poolName, namespace, ttl, &oidcCfg)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error allocating worker: %v\n", err)
		os.Exit(1)
	}

	fmt.Printf("✓ Allocated worker: %s\n", resp.WorkerName)
	fmt.Printf("  Token expires: %s\n", resp.ExpiresAt)

	// Create temp directory for certs
	certDir, err := os.MkdirTemp("", "bkctl-certs-*")
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error creating cert directory: %v\n", err)
		releaseWorker(resp.Token, &oidcCfg)
		os.Exit(1)
	}
	defer os.RemoveAll(certDir)

	// Write certificates
	if err := writeCerts(certDir, resp); err != nil {
		fmt.Fprintf(os.Stderr, "Error writing certificates: %v\n", err)
		releaseWorker(resp.Token, &oidcCfg)
		os.Exit(1)
	}

	fmt.Printf("✓ Certificates saved to %s\n", certDir)

	// Determine endpoint to use
	// Check for local endpoint override (for development/testing)
	endpoint := os.Getenv("BKCTL_GATEWAY_ENDPOINT")
	if endpoint == "" {
		endpoint = resp.GatewayEndpoint
		if endpoint == "" {
			endpoint = resp.Endpoint
		}
	}
	// Convert tcp:// to just host:port for docker buildx
	// Preserve hostname for SNI matching (important for TLS passthrough via Gateway API)
	endpoint = strings.TrimPrefix(endpoint, "tcp://")

	// Create builder name
	builderName := fmt.Sprintf("bkctl-%s", resp.WorkerName)

	// Setup buildx builder
	fmt.Printf("🔧 Setting up buildx builder '%s'...\n", builderName)
	if err := setupBuilder(builderName, endpoint, certDir); err != nil {
		fmt.Fprintf(os.Stderr, "Error setting up builder: %v\n", err)
		releaseWorker(resp.Token, &oidcCfg)
		os.Exit(1)
	}

	// Handle cleanup on interrupt
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	go func() {
		<-sigCh
		fmt.Println("\n⚠️  Interrupted, cleaning up...")
		removeBuilder(builderName)
		releaseWorker(resp.Token, &oidcCfg)
		cancel()
		os.Exit(130)
	}()

	// Run docker buildx build
	fmt.Printf("🏗️  Running build...\n")
	if err := runDockerBuild(ctx, builderName, dockerArgs); err != nil {
		fmt.Fprintf(os.Stderr, "Build failed: %v\n", err)
		removeBuilder(builderName)
		releaseWorker(resp.Token, &oidcCfg)
		os.Exit(1)
	}

	fmt.Println("✓ Build complete!")

	// Cleanup
	removeBuilder(builderName)
	releaseWorker(resp.Token, &oidcCfg)
}

func runAllocate(args []string) {
	poolName := ""
	namespace := getEnvOrDefault("BKCTL_NAMESPACE", defaultNamespace)
	ttl := "1h"
	outputJSON := false
	var oidcCfg oidcConfig

	for i := 0; i < len(args); i++ {
		switch args[i] {
		case "--pool", "-p":
			if i+1 < len(args) {
				poolName = args[i+1]
				i++
			}
		case "--namespace", "-n":
			if i+1 < len(args) {
				namespace = args[i+1]
				i++
			}
		case "--ttl":
			if i+1 < len(args) {
				ttl = args[i+1]
				i++
			}
		case "--json":
			outputJSON = true
		case "--oidc-actor":
			if i+1 < len(args) {
				oidcCfg.actor = args[i+1]
				i++
			}
		case "--oidc-repository":
			if i+1 < len(args) {
				oidcCfg.repository = args[i+1]
				i++
			}
		case "--oidc-subject":
			if i+1 < len(args) {
				oidcCfg.subject = args[i+1]
				i++
			}
		}
	}

	if poolName == "" {
		fmt.Fprintln(os.Stderr, "Error: --pool is required")
		os.Exit(1)
	}

	resp, err := allocateWorker(poolName, namespace, ttl, &oidcCfg)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error allocating worker: %v\n", err)
		os.Exit(1)
	}

	if outputJSON {
		enc := json.NewEncoder(os.Stdout)
		enc.SetIndent("", "  ")
		enc.Encode(resp)
	} else {
		fmt.Printf("Worker allocated successfully!\n\n")
		fmt.Printf("Worker:    %s\n", resp.WorkerName)
		fmt.Printf("Token:     %s\n", resp.Token)
		fmt.Printf("Endpoint:  %s\n", resp.GatewayEndpoint)
		fmt.Printf("Expires:   %s\n", resp.ExpiresAt)
		fmt.Printf("\nTo release: bkctl release --token %s\n", resp.Token)
	}
}

func runRelease(args []string) {
	token := ""
	var oidcCfg oidcConfig

	for i := 0; i < len(args); i++ {
		switch args[i] {
		case "--token", "-t":
			if i+1 < len(args) {
				token = args[i+1]
				i++
			}
		case "--oidc-actor":
			if i+1 < len(args) {
				oidcCfg.actor = args[i+1]
				i++
			}
		case "--oidc-repository":
			if i+1 < len(args) {
				oidcCfg.repository = args[i+1]
				i++
			}
		case "--oidc-subject":
			if i+1 < len(args) {
				oidcCfg.subject = args[i+1]
				i++
			}
		}
	}

	if token == "" {
		fmt.Fprintln(os.Stderr, "Error: --token is required")
		os.Exit(1)
	}

	if err := releaseWorker(token, &oidcCfg); err != nil {
		fmt.Fprintf(os.Stderr, "Error releasing worker: %v\n", err)
		os.Exit(1)
	}

	fmt.Println("Worker released successfully")
}

func runStatus(args []string) {
	poolName := ""
	namespace := getEnvOrDefault("BKCTL_NAMESPACE", defaultNamespace)
	var oidcCfg oidcConfig

	for i := 0; i < len(args); i++ {
		switch args[i] {
		case "--pool", "-p":
			if i+1 < len(args) {
				poolName = args[i+1]
				i++
			}
		case "--namespace", "-n":
			if i+1 < len(args) {
				namespace = args[i+1]
				i++
			}
		case "--oidc-actor":
			if i+1 < len(args) {
				oidcCfg.actor = args[i+1]
				i++
			}
		case "--oidc-repository":
			if i+1 < len(args) {
				oidcCfg.repository = args[i+1]
				i++
			}
		case "--oidc-subject":
			if i+1 < len(args) {
				oidcCfg.subject = args[i+1]
				i++
			}
		}
	}

	if poolName == "" {
		fmt.Fprintln(os.Stderr, "Error: --pool is required")
		os.Exit(1)
	}

	endpoint := getEnvOrDefault("BKCTL_ENDPOINT", defaultControllerEndpoint)
	url := fmt.Sprintf("%s/api/v1/pools/%s?namespace=%s", endpoint, poolName, namespace)

	req, _ := http.NewRequest(http.MethodGet, url, nil)
	if err := addAuthHeader(req, &oidcCfg); err != nil {
		fmt.Fprintf(os.Stderr, "Error: %v\n", err)
		os.Exit(1)
	}

	client := createHTTPClient(10 * time.Second)
	resp, err := client.Do(req)
	if err != nil {
		fmt.Fprintf(os.Stderr, "Error getting pool status: %v\n", err)
		os.Exit(1)
	}
	defer resp.Body.Close()

	body, _ := io.ReadAll(resp.Body)

	if resp.StatusCode != http.StatusOK {
		fmt.Fprintf(os.Stderr, "Error: %s\n", string(body))
		os.Exit(1)
	}

	// Pretty print the JSON
	var prettyJSON bytes.Buffer
	json.Indent(&prettyJSON, body, "", "  ")
	fmt.Println(prettyJSON.String())
}

func allocateWorker(poolName, namespace, ttl string, oidcCfg *oidcConfig) (*allocateResponse, error) {
	endpoint := getEnvOrDefault("BKCTL_ENDPOINT", defaultControllerEndpoint)
	url := fmt.Sprintf("%s/api/v1/workers/allocate", endpoint)

	reqBody, _ := json.Marshal(map[string]string{
		"poolName":  poolName,
		"namespace": namespace,
		"ttl":       ttl,
	})

	req, err := http.NewRequest(http.MethodPost, url, bytes.NewReader(reqBody))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	if err := addAuthHeader(req, oidcCfg); err != nil {
		return nil, fmt.Errorf("authentication failed: %w", err)
	}

	client := createHTTPClient(5 * time.Minute) // Long timeout for scaling
	resp, err := client.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		body, _ := io.ReadAll(resp.Body)
		return nil, fmt.Errorf("allocation failed: %s", string(body))
	}

	var result allocateResponse
	if err := json.NewDecoder(resp.Body).Decode(&result); err != nil {
		return nil, fmt.Errorf("failed to decode response: %w", err)
	}

	return &result, nil
}

func releaseWorker(token string, oidcCfg *oidcConfig) error {
	endpoint := getEnvOrDefault("BKCTL_ENDPOINT", defaultControllerEndpoint)
	url := fmt.Sprintf("%s/api/v1/workers/release", endpoint)

	reqBody, _ := json.Marshal(map[string]string{"token": token})

	req, err := http.NewRequest(http.MethodPost, url, bytes.NewReader(reqBody))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")
	if err := addAuthHeader(req, oidcCfg); err != nil {
		return fmt.Errorf("authentication failed: %w", err)
	}

	client := createHTTPClient(10 * time.Second)
	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK && resp.StatusCode != http.StatusNotFound {
		body, _ := io.ReadAll(resp.Body)
		return fmt.Errorf("release failed: %s", string(body))
	}

	return nil
}

func writeCerts(certDir string, resp *allocateResponse) error {
	// Decode base64 certs
	caCert, err := base64.StdEncoding.DecodeString(resp.CACert)
	if err != nil {
		return fmt.Errorf("failed to decode CA cert: %w", err)
	}

	clientCert, err := base64.StdEncoding.DecodeString(resp.ClientCert)
	if err != nil {
		return fmt.Errorf("failed to decode client cert: %w", err)
	}

	clientKey, err := base64.StdEncoding.DecodeString(resp.ClientKey)
	if err != nil {
		return fmt.Errorf("failed to decode client key: %w", err)
	}

	// Write files
	if err := os.WriteFile(filepath.Join(certDir, "ca.crt"), caCert, 0600); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(certDir, "client.crt"), clientCert, 0600); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(certDir, "client.key"), clientKey, 0600); err != nil {
		return err
	}

	return nil
}

func setupBuilder(name, endpoint, certDir string) error {
	// Check if builder exists and remove it
	checkCmd := exec.Command("docker", "buildx", "inspect", name)
	if checkCmd.Run() == nil {
		// Builder exists, remove it
		rmCmd := exec.Command("docker", "buildx", "rm", name, "--force")
		rmCmd.Run()
	}

	// Create new builder with TLS certs
	args := []string{
		"buildx", "create",
		"--name", name,
		"--driver", "remote",
		"--driver-opt", fmt.Sprintf("cacert=%s,cert=%s,key=%s",
			filepath.Join(certDir, "ca.crt"),
			filepath.Join(certDir, "client.crt"),
			filepath.Join(certDir, "client.key"),
		),
		fmt.Sprintf("tcp://%s", endpoint),
	}

	cmd := exec.Command("docker", args...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr

	return cmd.Run()
}

func removeBuilder(name string) {
	cmd := exec.Command("docker", "buildx", "rm", name, "--force")
	cmd.Run() // Ignore errors
}

func runDockerBuild(ctx context.Context, builderName string, args []string) error {
	// Prepend --builder flag
	fullArgs := append([]string{"buildx", "build", "--builder", builderName}, args...)

	cmd := exec.CommandContext(ctx, "docker", fullArgs...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	cmd.Stdin = os.Stdin

	return cmd.Run()
}

func addAuthHeader(req *http.Request, oidcCfg *oidcConfig) error {
	// First, check if token is explicitly set
	token := os.Getenv("BKCTL_TOKEN")
	if token != "" {
		req.Header.Set("Authorization", "Bearer "+token)
		return nil
	}

	// If OIDC config provided, try to generate token
	// Only generate if at least one OIDC flag is explicitly provided
	if oidcCfg != nil && (oidcCfg.actor != "" || oidcCfg.repository != "" || oidcCfg.issuer != "" || oidcCfg.subject != "" || oidcCfg.audience != "") {
		namespace := getEnvOrDefault("BKCTL_NAMESPACE", defaultNamespace)

		// Require at least actor or repository to be set (not just defaults)
		if oidcCfg.actor == "" && oidcCfg.repository == "" && oidcCfg.issuer == "" {
			return fmt.Errorf("OIDC authentication requires at least --oidc-actor, --oidc-repository, or --oidc-issuer. Set BKCTL_TOKEN or provide explicit OIDC flags")
		}

		// Use provided values, with minimal defaults only for required fields
		issuer := oidcCfg.issuer
		if issuer == "" {
			// Auto-detect mock-oidc only if user provided actor/repo (shows intent)
			if oidcCfg.actor != "" || oidcCfg.repository != "" {
				issuer = "http://mock-oidc." + namespace + ".svc:8888"
			} else {
				return fmt.Errorf("OIDC issuer required. Set --oidc-issuer or provide --oidc-actor/--oidc-repository")
			}
		}

		// Use actor as subject if subject not provided
		subject := oidcCfg.subject
		if subject == "" && oidcCfg.actor != "" {
			subject = oidcCfg.actor
		}
		if subject == "" {
			subject = "test-user" // Minimal default only if nothing provided
		}

		audience := oidcCfg.audience
		if audience == "" {
			audience = "buildkit-controller"
		}

		actor := oidcCfg.actor
		if actor == "" {
			actor = "test-user" // Minimal default only if nothing provided
		}

		repository := oidcCfg.repository
		if repository == "" {
			repository = "test-org/test-repo" // Minimal default only if nothing provided
		}

		generatedToken, err := getOIDCTokenFromCluster(
			issuer,
			subject,
			audience,
			actor,
			repository,
			namespace,
		)
		if err == nil && generatedToken != "" {
			req.Header.Set("Authorization", "Bearer "+generatedToken)
			return nil
		}

		// If cluster token generation failed, return error
		return fmt.Errorf("failed to generate OIDC token: %w. Request will fail if pool requires OIDC authentication", err)
	}

	// No OIDC config and no token - return error
	// Allow override via environment variable for dev/testing
	if os.Getenv("BKCTL_ALLOW_NO_AUTH") == "true" {
		fmt.Fprintf(os.Stderr, "Warning: No authentication provided (BKCTL_ALLOW_NO_AUTH=true). This may only work if controller is in dev mode.\n")
		return nil
	}

	return fmt.Errorf("no authentication provided. Set BKCTL_TOKEN or provide OIDC flags (--oidc-actor, --oidc-repository). Set BKCTL_ALLOW_NO_AUTH=true to allow (dev mode only)")
}

func runOIDCToken(args []string) {
	oidcCfg := oidcConfig{
		issuer:     "http://mock-oidc.buildkit-system.svc:8888",
		actor:      "test-user",
		repository: "test-org/test-repo",
		subject:    "test-user",
		audience:   "buildkit-controller",
	}

	for i := 0; i < len(args); i++ {
		switch args[i] {
		case "--issuer":
			if i+1 < len(args) {
				oidcCfg.issuer = args[i+1]
				i++
			}
		case "--actor", "--oidc-actor":
			if i+1 < len(args) {
				oidcCfg.actor = args[i+1]
				i++
			}
		case "--repository", "--oidc-repository":
			if i+1 < len(args) {
				oidcCfg.repository = args[i+1]
				i++
			}
		case "--subject", "--oidc-subject":
			if i+1 < len(args) {
				oidcCfg.subject = args[i+1]
				i++
			}
		case "--audience":
			if i+1 < len(args) {
				oidcCfg.audience = args[i+1]
				i++
			}
		}
	}

	// Try to get token from in-cluster mock-oidc service
	namespace := getEnvOrDefault("BKCTL_NAMESPACE", defaultNamespace)
	token, err := getOIDCTokenFromCluster(oidcCfg.issuer, oidcCfg.subject, oidcCfg.audience, oidcCfg.actor, oidcCfg.repository, namespace)
	if err != nil {
		// Fallback: try local mock-oidc binary
		token, err = getOIDCTokenFromLocal(oidcCfg.issuer, oidcCfg.subject, oidcCfg.audience, oidcCfg.actor, oidcCfg.repository)
		if err != nil {
			fmt.Fprintf(os.Stderr, "Error generating OIDC token: %v\n", err)
			fmt.Fprintf(os.Stderr, "Make sure mock-oidc is running in cluster or locally\n")
			os.Exit(1)
		}
	}

	fmt.Println(token)
}

func getOIDCTokenFromCluster(issuer, subject, audience, actor, repository, namespace string) (string, error) {
	// Default issuer if not set
	if issuer == "" {
		issuer = fmt.Sprintf("http://mock-oidc.%s.svc:8888", namespace)
	}
	if subject == "" {
		subject = "test-user"
	}
	if audience == "" {
		audience = "buildkit-controller"
	}
	if actor == "" {
		actor = "test-user"
	}
	if repository == "" {
		repository = "test-org/test-repo"
	}

	// Use kubectl exec to call the in-cluster mock-oidc service
	cmd := exec.Command("kubectl", "exec", "-n", namespace, "deploy/mock-oidc", "--",
		"wget", "-qO-",
		"--post-data", fmt.Sprintf(`{"sub":"%s","aud":"%s","claims":{"actor":"%s","repository":"%s"}}`, subject, audience, actor, repository),
		"--header=Content-Type: application/json",
		fmt.Sprintf("%s/token", issuer))

	output, err := cmd.Output()
	if err != nil {
		return "", err
	}

	var result struct {
		IDToken string `json:"id_token"`
	}
	if err := json.Unmarshal(output, &result); err != nil {
		return "", fmt.Errorf("failed to parse token response: %w", err)
	}

	return result.IDToken, nil
}

func getOIDCTokenFromLocal(issuer, subject, audience, actor, repository string) (string, error) {
	// Try to use local mock-oidc binary
	// This assumes mock-oidc is in PATH or can be found
	cmd := exec.Command("mock-oidc", "--print-token",
		"--issuer", issuer,
		"--subject", subject,
		"--audience", audience,
		"--actor", actor,
		"--repository", repository)

	output, err := cmd.Output()
	if err != nil {
		return "", err
	}

	return strings.TrimSpace(string(output)), nil
}

// createHTTPClient creates an HTTP client with TLS configuration based on environment variables.
// It supports:
//   - BKCTL_TLS_SKIP_VERIFY: Skip TLS certificate verification (set to "true")
//   - BKCTL_TLS_CA_CERT: Path to CA certificate file for custom CA trust
func createHTTPClient(timeout time.Duration) *http.Client {
	transport := &http.Transport{}

	// Check if we should skip TLS verification
	skipVerify := os.Getenv("BKCTL_TLS_SKIP_VERIFY") == "true"

	// Check if a custom CA cert is provided
	caCertPath := os.Getenv("BKCTL_TLS_CA_CERT")

	if skipVerify || caCertPath != "" {
		tlsConfig := &tls.Config{
			InsecureSkipVerify: skipVerify,
		}

		// If CA cert is provided, load it
		if caCertPath != "" {
			caCert, err := os.ReadFile(caCertPath)
			if err != nil {
				fmt.Fprintf(os.Stderr, "Warning: Failed to read CA cert from %s: %v\n", caCertPath, err)
			} else {
				caPool := x509.NewCertPool()
				if !caPool.AppendCertsFromPEM(caCert) {
					fmt.Fprintf(os.Stderr, "Warning: Failed to parse CA cert from %s\n", caCertPath)
				} else {
					tlsConfig.RootCAs = caPool
				}
			}
		}

		transport.TLSClientConfig = tlsConfig
	}

	return &http.Client{
		Transport: transport,
		Timeout:   timeout,
	}
}

func getEnvOrDefault(key, defaultValue string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return defaultValue
}
