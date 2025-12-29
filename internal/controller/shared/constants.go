package shared

import "time"

const (
	// DefaultRequeueInterval is the default interval for requeuing reconciliation.
	DefaultRequeueInterval = 30 * time.Second

	// MinRequeueInterval is the minimum interval for requeuing (1 hour).
	MinRequeueInterval = 1 * time.Hour

	// MaxRequeueInterval is the maximum interval for requeuing (24 hours).
	MaxRequeueInterval = 24 * time.Hour

	// StatusUpdateInterval is the interval for periodic status updates (30 seconds).
	StatusUpdateInterval = 30 * time.Second

	// DefaultCertRenewalTime is the default time before certificate expiry to renew (30 days).
	DefaultCertRenewalTime = 720 * time.Hour

	// DefaultCertDuration is the default certificate duration (1 year).
	DefaultCertDuration = 8760 * time.Hour

	// WorkerStuckThreshold is the time after which a worker is considered stuck in provisioning.
	WorkerStuckThreshold = 10 * time.Minute

	// DefaultMaxWorkers is the default maximum number of workers if not specified.
	DefaultMaxWorkers = int32(10)

	// DefaultGatewayPort is the default port for the gateway service.
	DefaultGatewayPort = int32(1235)

	// RetryBackoffBaseDelay is the base delay for retry backoff (100ms).
	RetryBackoffBaseDelay = 100 * time.Millisecond

	// RetryMaxAttempts is the maximum number of retry attempts.
	RetryMaxAttempts = 3

	// CertRenewalCheckWindow is the time window for checking certificate renewal (30 days).
	CertRenewalCheckWindow = 30 * 24 * time.Hour

	// ScaleDownScheduleCheckWindow is the time window for checking scale-down schedules (2 minutes).
	ScaleDownScheduleCheckWindow = 2 * time.Minute

	// WorkerProvisioningRequeueInterval is the interval for requeuing workers in provisioning phase.
	WorkerProvisioningRequeueInterval = 5 * time.Second
)
