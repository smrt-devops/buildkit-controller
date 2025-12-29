// Package main provides a mock OIDC server for local testing.
// THIS IS FOR DEVELOPMENT/TESTING ONLY - DO NOT USE IN PRODUCTION.
package main

import (
	"context"
	"flag"
	"fmt"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/smrt-devops/buildkit-controller/internal/auth"
)

func main() {
	var port int
	var issuer string
	var printToken bool
	var tokenSubject string
	var tokenAudience string
	var tokenActor string
	var tokenRepo string

	flag.IntVar(&port, "port", 8888, "Port to listen on")
	flag.StringVar(&issuer, "issuer", "", "Issuer URL (defaults to http://localhost:<port>)")
	flag.BoolVar(&printToken, "print-token", false, "Print a sample token and exit")
	flag.StringVar(&tokenSubject, "subject", "test-user", "Token subject (for --print-token)")
	flag.StringVar(&tokenAudience, "audience", "buildkit-controller", "Token audience (for --print-token)")
	flag.StringVar(&tokenActor, "actor", "test-actor", "Token actor claim (for --print-token)")
	flag.StringVar(&tokenRepo, "repository", "test-org/test-repo", "Token repository claim (for --print-token)")
	flag.Parse()

	if issuer == "" {
		issuer = fmt.Sprintf("http://localhost:%d", port)
	}

	mockServer, err := auth.NewMockOIDCServer(issuer, port)
	if err != nil {
		log.Fatalf("Failed to create mock OIDC server: %v", err)
	}

	// If just printing a token, do that and exit
	if printToken {
		token, err := mockServer.GenerateToken(tokenSubject, tokenAudience, 1*time.Hour, map[string]string{
			"actor":      tokenActor,
			"repository": tokenRepo,
			"ref":        "refs/heads/main",
		})
		if err != nil {
			log.Fatalf("Failed to generate token: %v", err)
		}
		fmt.Println(token)
		return
	}

	// Start server
	fmt.Printf("Starting mock OIDC server...\n")
	fmt.Printf("  Issuer:    %s\n", issuer)
	fmt.Printf("  Port:      %d\n", port)
	fmt.Printf("\n")
	fmt.Printf("Endpoints:\n")
	fmt.Printf("  Discovery: %s/.well-known/openid-configuration\n", issuer)
	fmt.Printf("  JWKS:      %s/.well-known/jwks.json\n", issuer)
	fmt.Printf("  Token:     %s/token (POST)\n", issuer)
	fmt.Printf("\n")
	fmt.Printf("To generate a test token:\n")
	fmt.Printf("  curl -X POST %s/token -H 'Content-Type: application/json' \\\n", issuer)
	fmt.Printf("    -d '{\"sub\": \"test-user\", \"aud\": \"buildkit-controller\", \"claims\": {\"actor\": \"my-actor\", \"repository\": \"my-org/my-repo\"}}'\n")
	fmt.Printf("\n")
	fmt.Printf("Or use the CLI:\n")
	fmt.Printf("  go run ./cmd/mock-oidc --print-token --actor my-actor --repository my-org/my-repo\n")
	fmt.Printf("\n")
	fmt.Printf("Press Ctrl+C to stop\n")

	// Handle shutdown
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)

	go func() {
		<-sigCh
		fmt.Println("\nShutting down...")
		shutdownCtx, shutdownCancel := context.WithTimeout(ctx, 5*time.Second)
		defer shutdownCancel()
		if err := mockServer.Stop(shutdownCtx); err != nil {
			log.Printf("Shutdown error: %v", err)
		}
	}()

	if err := mockServer.Start(); err != nil && err != http.ErrServerClosed {
		log.Fatalf("Server error: %v", err)
	}
}
