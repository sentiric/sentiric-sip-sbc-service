// src/telemetry.rs
use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt;
use tracing::{Event, Subscriber};
use tracing_subscriber::fmt::{format::Writer, FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

/// SUTS v4.0 Log Record (Sovereign Edition)
#[derive(Serialize)]
struct SutsLogRecord<'a> {
    schema_v: &'static str,
    ts: String,
    severity: String,
    tenant_id: String,

    resource: ResourceContext,

    trace_id: Option<String>,
    span_id: Option<String>,

    event: String,
    message: String,

    attributes: HashMap<String, Value>,

    #[serde(skip)]
    _marker: std::marker::PhantomData<&'a ()>,
}

#[derive(Serialize, Clone)]
struct ResourceContext {
    #[serde(rename = "service.name")]
    service_name: String,
    #[serde(rename = "service.version")]
    service_version: String,
    #[serde(rename = "service.env")]
    service_env: String,
    #[serde(rename = "host.name")]
    host_name: String,
}

pub struct SutsFormatter {
    resource: ResourceContext,
    tenant_id: String, // [ARCH-COMPLIANCE] Dinamik tenant_id
}

impl SutsFormatter {
    pub fn new(
        service_name: String,
        version: String,
        env: String,
        host_name: String,
        tenant_id: String,
    ) -> Self {
        Self {
            resource: ResourceContext {
                service_name,
                service_version: version,
                service_env: env,
                host_name,
            },
            tenant_id,
        }
    }
}

impl<S, N> FormatEvent<S, N> for SutsFormatter
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let meta = event.metadata();
        let ts = Utc::now().to_rfc3339();

        let severity = match *meta.level() {
            tracing::Level::ERROR => "ERROR",
            tracing::Level::WARN => "WARN",
            tracing::Level::INFO => "INFO",
            tracing::Level::DEBUG => "DEBUG",
            tracing::Level::TRACE => "DEBUG",
        }
        .to_string();

        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);

        let event_name = visitor
            .fields
            .remove("event")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "LOG_EVENT".to_string());

        let message = visitor
            .fields
            .remove("message")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(String::new);

        // [CLIPPY FIX]: manual_map
        let trace_id = visitor
            .fields
            .get("trace_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                visitor
                    .fields
                    .get("sip.call_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            });

        // [ARCH-COMPLIANCE] span_id ihlali düzeltildi. Aktif context'ten span_id alınıyor.
        let span_id = ctx
            .lookup_current()
            .map(|span| format!("{:x}", span.id().into_u64()));

        let log_record = SutsLogRecord {
            schema_v: "1.0.0",
            ts,
            severity,
            tenant_id: self.tenant_id.clone(), // [ARCH-COMPLIANCE] Hardcoded string kaldırıldı
            resource: self.resource.clone(),
            trace_id,
            span_id,
            event: event_name,
            message,
            attributes: visitor.fields,
            _marker: std::marker::PhantomData,
        };

        if let Ok(json_str) = serde_json::to_string(&log_record) {
            writeln!(writer, "{}", json_str)?;
        }

        Ok(())
    }
}

#[derive(Default)]
struct JsonVisitor {
    fields: HashMap<String, Value>,
}

impl tracing::field::Visit for JsonVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.fields.insert(
            field.name().to_string(),
            Value::String(format!("{:?}", value)),
        );
    }
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), Value::Bool(value));
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.fields.insert(field.name().to_string(), json!(value));
    }
    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.fields
            .insert(field.name().to_string(), Value::String(value.to_string()));
    }
}
