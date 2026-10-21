//! JSON log encoding with the spec's standard fields.
//!
//! One object per line: `timestamp`, `level`, `target`, `message`, `replica`,
//! then the event's fields and the fields of every enclosing span (inner
//! spans win). Never log secrets; this layer does not filter them.

use std::fmt;

use serde_json::{Map, Value};
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{FormatEvent, FormatFields, JsonFields, Writer};
use tracing_subscriber::fmt::time::{FormatTime, SystemTime};
use tracing_subscriber::fmt::{FmtContext, FormattedFields};
use tracing_subscriber::registry::LookupSpan;

use crate::config::LogFormat;

#[derive(Default)]
struct Collect(Map<String, Value>);

impl Visit for Collect {
    fn record_debug(&mut self, f: &Field, v: &dyn fmt::Debug) {
        self.0
            .insert(f.name().into(), Value::String(format!("{v:?}")));
    }
    fn record_str(&mut self, f: &Field, v: &str) {
        self.0.insert(f.name().into(), Value::String(v.into()));
    }
    fn record_i64(&mut self, f: &Field, v: i64) {
        self.0.insert(f.name().into(), v.into());
    }
    fn record_u64(&mut self, f: &Field, v: u64) {
        self.0.insert(f.name().into(), v.into());
    }
    fn record_bool(&mut self, f: &Field, v: bool) {
        self.0.insert(f.name().into(), v.into());
    }
}

/// Formats events as flat JSON objects carrying `replica`.
pub struct JsonWithReplica {
    replica: String,
}

impl<S, N> FormatEvent<S, N> for JsonWithReplica
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut w: Writer<'_>,
        ev: &Event<'_>,
    ) -> fmt::Result {
        let mut out = Map::new();
        let mut ts = String::new();
        SystemTime.format_time(&mut Writer::new(&mut ts))?;
        out.insert("timestamp".into(), ts.into());
        out.insert(
            "level".into(),
            ev.metadata().level().as_str().to_lowercase().into(),
        );
        out.insert("target".into(), ev.metadata().target().into());
        out.insert("replica".into(), self.replica.clone().into());
        let mut spans = Map::new();
        if let Some(scope) = ctx.event_scope() {
            for span in scope.from_root() {
                let ext = span.extensions();
                if let Some(f) = ext.get::<FormattedFields<N>>()
                    && let Ok(Value::Object(m)) = serde_json::from_str::<Value>(f.fields.as_str())
                {
                    spans.extend(m);
                }
            }
        }
        let mut fields = Collect::default();
        ev.record(&mut fields);
        out.extend(spans);
        out.extend(fields.0);
        writeln!(w, "{}", Value::Object(out))
    }
}

/// Install the global subscriber.
pub fn init(level: &str, format: LogFormat, replica: &str) -> anyhow::Result<()> {
    let filter =
        EnvFilter::try_new(level).map_err(|e| anyhow::anyhow!("bad log level `{level}`: {e}"))?;
    let b = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr);
    match format {
        LogFormat::Json => b
            .fmt_fields(JsonFields::new())
            .event_format(JsonWithReplica {
                replica: replica.to_owned(),
            })
            .try_init(),
        LogFormat::Text => b.try_init(),
    }
    .map_err(|e| anyhow::anyhow!("init tracing: {e}"))
}
