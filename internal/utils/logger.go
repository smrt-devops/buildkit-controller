package utils

import (
	"os"
	"strings"

	"github.com/go-logr/logr"
	"sigs.k8s.io/controller-runtime/pkg/log/zap"
)

// Logger is an alias for logr.Logger to centralize logging imports.
// All packages should use utils.Logger instead of importing logr directly.
type Logger = logr.Logger

// LogLevel represents a log level.
type LogLevel string

const (
	LogLevelDebug LogLevel = "debug"
	LogLevelInfo  LogLevel = "info"
	LogLevelWarn  LogLevel = "warn"
	LogLevelError LogLevel = "error"
)

// LogFormat represents a log format.
type LogFormat string

const (
	LogFormatJSON LogFormat = "json"
	LogFormatText LogFormat = "text"
)

// LoggerConfig holds logger configuration.
type LoggerConfig struct {
	Level       LogLevel
	Format      LogFormat
	Development bool
}

// DefaultLoggerConfig returns the default logger configuration.
func DefaultLoggerConfig() *LoggerConfig {
	return &LoggerConfig{
		Level:       LogLevelInfo,
		Format:      LogFormatJSON,
		Development: false,
	}
}

// LoadLoggerConfigFromEnv loads logger configuration from environment variables.
func LoadLoggerConfigFromEnv() *LoggerConfig {
	config := DefaultLoggerConfig()

	if level := os.Getenv("LOG_LEVEL"); level != "" {
		config.Level = LogLevel(strings.ToLower(level))
	}

	if format := os.Getenv("LOG_FORMAT"); format != "" {
		config.Format = LogFormat(strings.ToLower(format))
	}

	if dev := os.Getenv("LOG_DEVELOPMENT"); dev != "" {
		config.Development = strings.EqualFold(dev, "true")
	}

	return config
}

// NewLogger creates a new Logger with the given configuration.
// This uses zap as the backend, which is compatible with controller-runtime.
func NewLogger(config *LoggerConfig) Logger {
	if config == nil {
		config = DefaultLoggerConfig()
	}

	opts := zap.Options{
		Development: config.Development,
	}

	// Set encoding based on format
	if config.Format == LogFormatText {
		opts.Development = true // Text-like console encoder
	} else {
		opts.Development = false // JSON encoder
	}

	// Note: Log level is controlled via zap's verbosity levels
	// Debug = V(1), Info = V(0), Warn/Error are Info with different levels
	// The actual level filtering is handled by zap internally based on Development mode

	return zap.New(zap.UseFlagOptions(&opts))
}

// NewLoggerFromEnv creates a new logger using environment variable configuration.
func NewLoggerFromEnv() Logger {
	return NewLogger(LoadLoggerConfigFromEnv())
}

// LoggerHelper provides convenience methods for working with Logger.
type LoggerHelper struct {
	Logger
}

// NewLoggerHelper wraps a Logger with convenience methods.
func NewLoggerHelper(logger Logger) *LoggerHelper {
	return &LoggerHelper{Logger: logger}
}

// WithComponent adds a component name to the logger.
func (h *LoggerHelper) WithComponent(name string) *LoggerHelper {
	return &LoggerHelper{Logger: h.WithName(name)}
}

// WithValues adds key-value pairs to the logger.
func (h *LoggerHelper) WithValues(keysAndValues ...interface{}) *LoggerHelper {
	return &LoggerHelper{Logger: h.Logger.WithValues(keysAndValues...)}
}

// Debug logs at debug level (V(1)).
func (h *LoggerHelper) Debug(msg string, keysAndValues ...interface{}) {
	h.Logger.V(1).Info(msg, keysAndValues...)
}

// Warn logs at warn level.
func (h *LoggerHelper) Warn(msg string, keysAndValues ...interface{}) {
	h.Logger.Info(msg, keysAndValues...)
}

// Error logs an error.
func (h *LoggerHelper) Error(err error, msg string, keysAndValues ...interface{}) {
	h.Logger.Error(err, msg, keysAndValues...)
}

// Info logs at info level.
func (h *LoggerHelper) Info(msg string, keysAndValues ...interface{}) {
	h.Logger.Info(msg, keysAndValues...)
}
