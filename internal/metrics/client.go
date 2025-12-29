package metrics

import (
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"
)

var (
	// PoolsTotal is the total number of pools.
	PoolsTotal = promauto.NewGauge(prometheus.GaugeOpts{
		Name: "buildkit_controller_pools_total",
		Help: "Total number of BuildKit pools",
	})

	// WorkersTotal is the total number of workers.
	WorkersTotal = promauto.NewGaugeVec(prometheus.GaugeOpts{
		Name: "buildkit_controller_workers_total",
		Help: "Total number of BuildKit workers",
	}, []string{"pool", "namespace", "status"})

	// CertificatesIssued is the total number of certificates issued.
	CertificatesIssued = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "buildkit_controller_certificates_issued_total",
		Help: "Total number of certificates issued",
	}, []string{"type"})

	// CertificateExpirations tracks certificate expiration times.
	CertificateExpirations = promauto.NewGaugeVec(prometheus.GaugeOpts{
		Name: "buildkit_controller_certificate_expiry_seconds",
		Help: "Certificate expiry time in seconds since epoch",
	}, []string{"pool", "type"})

	// APIRequestsTotal is the total number of API requests.
	APIRequestsTotal = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "buildkit_controller_api_requests_total",
		Help: "Total number of API requests",
	}, []string{"endpoint", "method", "status"})

	// APIRequestDuration is the duration of API requests.
	APIRequestDuration = promauto.NewHistogramVec(prometheus.HistogramOpts{
		Name:    "buildkit_controller_api_request_duration_seconds",
		Help:    "Duration of API requests in seconds",
		Buckets: prometheus.DefBuckets,
	}, []string{"endpoint", "method"})

	// OIDCVerificationsTotal is the total number of OIDC verifications.
	OIDCVerificationsTotal = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "buildkit_controller_oidc_verifications_total",
		Help: "Total number of OIDC token verifications",
	}, []string{"issuer", "result"})

	// ScaleOperationsTotal is the total number of scale operations.
	ScaleOperationsTotal = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "buildkit_controller_scale_operations_total",
		Help: "Total number of scaling operations",
	}, []string{"pool", "operation"})

	// ReconciliationsTotal is the total number of reconciliations.
	ReconciliationsTotal = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "buildkit_controller_reconciliations_total",
		Help: "Total number of reconciliation loops",
	}, []string{"controller", "result"})

	// ReconciliationDuration is the duration of reconciliation loops.
	ReconciliationDuration = promauto.NewHistogramVec(prometheus.HistogramOpts{
		Name:    "buildkit_controller_reconciliation_duration_seconds",
		Help:    "Duration of reconciliation loops in seconds",
		Buckets: prometheus.DefBuckets,
	}, []string{"controller"})
)
