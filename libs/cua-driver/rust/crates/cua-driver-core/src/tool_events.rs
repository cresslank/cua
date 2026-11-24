// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Cua AI, Inc.

//! Opt-in, content-free local tool logs. This owns a private SDK provider: no
//! global logger, tracing bridge, environment detector, or vendor telemetry.

use std::future::Future;
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use opentelemetry::logs::{LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry::{KeyValue, SpanId, TraceId};
use opentelemetry_http::{Bytes, HttpClient, HttpError, Request, Response};
use opentelemetry_otlp::{Protocol, WithExportConfig, WithHttpConfig};
use opentelemetry_sdk::logs::{
    BatchConfigBuilder, BatchLogProcessor, SdkLogger, SdkLoggerProvider,
};
use opentelemetry_sdk::Resource;

use crate::protocol::ToolResult;

const EXPORT_TIMEOUT: Duration = Duration::from_millis(250);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(750);
const QUEUE_SIZE: usize = 256;
const BATCH_SIZE: usize = 64;
const MAX_RESOURCE_BYTES: usize = 4096;
const MAX_RESOURCE_VALUE_BYTES: usize = 256;

#[derive(Clone, Copy)]
struct InvocationContext {
    trace: TraceId,
    span: SpanId,
}

tokio::task_local! {
    // Generated here only; neither protocol IDs nor caller session labels enter.
    static INVOCATION: InvocationContext;
}

pub(crate) struct Config {
    endpoint: String,
    resource: Resource,
}

impl Config {
    pub(crate) fn read(mut get: impl FnMut(&str) -> Option<String>) -> Option<Self> {
        if get("OTEL_LOGS_EXPORTER").as_deref().map(str::trim) != Some("otlp")
            || get("OTEL_SDK_DISABLED")
                .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"))
        {
            return None;
        }
        let protocol =
            get("OTEL_EXPORTER_OTLP_LOGS_PROTOCOL").or_else(|| get("OTEL_EXPORTER_OTLP_PROTOCOL"));
        if protocol
            .as_deref()
            .is_some_and(|p| p.trim() != "http/protobuf")
        {
            return None;
        }
        // A signal endpoint is used verbatim. A general endpoint gets the
        // standard signal path. Never let the SDK choose its implicit default.
        let endpoint = if let Some(endpoint) = get("OTEL_EXPORTER_OTLP_LOGS_ENDPOINT") {
            endpoint
        } else {
            format!(
                "{}/v1/logs",
                get("OTEL_EXPORTER_OTLP_ENDPOINT")?.trim_end_matches('/')
            )
        };
        if endpoint.len() > 2048 {
            return None;
        }
        let mut endpoint = url::Url::parse(&endpoint).ok()?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
        {
            return None;
        }
        if endpoint.path().is_empty() {
            endpoint.set_path("/");
        }
        let mut attrs = std::collections::BTreeMap::new();
        attrs.insert("service.name".to_owned(), "cua-driver".to_owned());
        attrs.insert(
            "service.instance.id".to_owned(),
            uuid::Uuid::new_v4().to_string(),
        );
        if let Some(raw) = get("OTEL_RESOURCE_ATTRIBUTES") {
            if raw.len() > MAX_RESOURCE_BYTES {
                return None;
            }
            // Explicit deployment identity only. Do not detect host state or
            // accept arbitrary resource keys (e.g. session, token, URL, text).
            for pair in raw.split(',').take(32) {
                let Some((key, value)) = pair.split_once('=') else {
                    continue;
                };
                let (key, value) = (key.trim(), value.trim());
                if resource_key(key) && resource_value(value) {
                    attrs.insert(key.to_owned(), value.to_owned());
                }
            }
        }
        if let Some(name) = get("OTEL_SERVICE_NAME").filter(|name| resource_value(name)) {
            attrs.insert("service.name".to_owned(), name);
        }
        attrs.insert(
            "service.version".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        );
        attrs.insert("host.arch".to_owned(), std::env::consts::ARCH.to_owned());
        attrs.insert("os.type".to_owned(), std::env::consts::OS.to_owned());
        attrs.insert(
            "cua.build.git_sha".to_owned(),
            crate::build_info::git_sha().to_owned(),
        );
        Some(Self {
            endpoint: endpoint.into(),
            resource: Resource::builder_empty()
                .with_attributes(
                    attrs
                        .into_iter()
                        .map(|(key, value)| KeyValue::new(key, value)),
                )
                .build(),
        })
    }
}

fn resource_key(key: &str) -> bool {
    matches!(
        key,
        "service.name"
            | "service.namespace"
            | "service.instance.id"
            | "host.name"
            | "host.id"
            | "deployment.environment"
            | "deployment.environment.name"
            | "loghouse.project"
            | "loghouse.repo"
            | "loghouse.source_unit"
    )
}

fn resource_value(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RESOURCE_VALUE_BYTES
        && !value.chars().any(char::is_control)
}

/// Adapt the existing HTTP stack to the standard OTLP encoder/exporter. The
/// SDK's default clients allow proxies/redirects and unbounded response reads.
#[derive(Debug)]
struct CollectorHttp(ureq::Agent);

#[async_trait]
impl HttpClient for CollectorHttp {
    async fn send_bytes(&self, request: Request<Bytes>) -> Result<Response<Bytes>, HttpError> {
        // Rebuild the request with only our fixed header. In particular, SDK
        // support for OTEL_EXPORTER_OTLP_*_HEADERS cannot export credentials or
        // arbitrary ambient header values through this content-free channel.
        let mut response = self
            .0
            .post(request.uri().to_string())
            .header("content-type", "application/x-protobuf")
            .send(request.body().as_ref())?;
        let status = response.status();
        let body = response
            .body_mut()
            .with_config()
            .limit(4096)
            .read_to_vec()?;
        Ok(Response::builder().status(status).body(Bytes::from(body))?)
    }
}

pub(crate) struct ToolEvents {
    provider: SdkLoggerProvider,
    logger: SdkLogger,
}

impl ToolEvents {
    pub(crate) fn configured() -> Option<Self> {
        Self::new(Config::read(|key| std::env::var(key).ok())?)
    }

    pub(crate) fn new(config: Config) -> Option<Self> {
        let http = CollectorHttp(
            ureq::Agent::config_builder()
                .proxy(None)
                .max_redirects(0)
                .timeout_global(Some(EXPORT_TIMEOUT))
                .user_agent(concat!(
                    "cua-driver/",
                    env!("CARGO_PKG_VERSION"),
                    " otlp-logs"
                ))
                .build()
                .new_agent(),
        );
        let exporter = opentelemetry_otlp::LogExporter::builder()
            .with_http()
            .with_http_client(http)
            .with_endpoint(config.endpoint)
            .with_protocol(Protocol::HttpBinary)
            .with_timeout(EXPORT_TIMEOUT)
            .build()
            .ok()?;
        let processor = BatchLogProcessor::builder(exporter)
            .with_batch_config(
                BatchConfigBuilder::default()
                    .with_max_queue_size(QUEUE_SIZE)
                    .with_max_export_batch_size(BATCH_SIZE)
                    .with_scheduled_delay(Duration::from_millis(200))
                    .build(),
            )
            .build();
        let provider = SdkLoggerProvider::builder()
            .with_resource(config.resource)
            .with_log_processor(processor)
            .build();
        let logger = provider.logger("cua-driver.tool-events");
        Some(Self { provider, logger })
    }

    pub(crate) async fn observe(
        &self,
        name: &str,
        known: bool,
        invocation: impl Future<Output = ToolResult>,
    ) -> ToolResult {
        let parent = INVOCATION.try_with(|parent| *parent).ok();
        let context = InvocationContext {
            trace: parent
                .map(|p| p.trace)
                .unwrap_or_else(|| TraceId::from_bytes(*uuid::Uuid::new_v4().as_bytes())),
            span: SpanId::from_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap()),
        };
        let mut completion = Completion {
            events: self,
            name: crate::tool::event_tool_name(name),
            context,
            parent,
            started: Instant::now(),
            finished: false,
        };
        let result = INVOCATION.scope(context, invocation).await;
        let (outcome, error) = classify(known, &result);
        completion.emit(outcome, error);
        completion.finished = true;
        result
    }

    pub(crate) fn shutdown(&self) {
        // One processor; shutdown signals it to export buffered records. A dead
        // collector cannot hold up host shutdown beyond this fixed deadline.
        let _ = self.provider.shutdown_with_timeout(SHUTDOWN_TIMEOUT);
    }
}

impl Drop for ToolEvents {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct Completion<'a> {
    events: &'a ToolEvents,
    name: &'a str,
    context: InvocationContext,
    parent: Option<InvocationContext>,
    started: Instant,
    finished: bool,
}

impl Completion<'_> {
    fn emit(&self, outcome: &'static str, error: &'static str) {
        let elapsed = self.started.elapsed();
        let mut record = self.events.logger.create_log_record();
        record.set_event_name("cua.tool.completed");
        record.set_timestamp(SystemTime::now());
        record.set_severity_number(if outcome == "success" {
            Severity::Info
        } else {
            Severity::Warn
        });
        record.set_trace_context(self.context.trace, self.context.span, None);
        record.add_attribute("cua.tool.name", self.name.to_owned());
        record.add_attribute("cua.tool.outcome", outcome);
        record.add_attribute("error.type", error);
        record.add_attribute(
            "cua.tool.duration_ns",
            elapsed.as_nanos().min(i64::MAX as u128) as i64,
        );
        if let Some(parent) = self.parent {
            record.add_attribute("cua.tool.parent_span_id", parent.span.to_string());
        }
        self.events.logger.emit(record);
    }
}

impl Drop for Completion<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.emit(
                if std::thread::panicking() {
                    "panicked"
                } else {
                    "cancelled"
                },
                "interrupted",
            );
        }
    }
}

fn classify(known: bool, result: &ToolResult) -> (&'static str, &'static str) {
    if !known {
        return ("error", "unknown_tool");
    }
    let structured = result.structured_content.as_ref();
    let field = |key| structured.and_then(|value| value.get(key));
    let code = field("code")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            field("refusal")
                .and_then(|refusal| refusal.get("code"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            field("error")
                .and_then(|error| error.get("code"))
                .and_then(serde_json::Value::as_str)
        });
    let refused = field("status").and_then(serde_json::Value::as_str) == Some("refused")
        || field("effect").and_then(serde_json::Value::as_str) == Some("refused");
    let failed = result.is_error == Some(true)
        || field("status").and_then(serde_json::Value::as_str) == Some("error")
        || field("ok").and_then(serde_json::Value::as_bool) == Some(false)
        || field("success").and_then(serde_json::Value::as_bool) == Some(false);
    if !failed && !refused {
        return ("success", "none");
    }
    let class = match code {
        Some("invalid_arguments" | "invalid_params") => "invalid_arguments",
        Some(
            "permission_denied"
            | "accessibility_permission_denied"
            | "screen_recording_permission_denied"
            | "tcc_permission_denied"
            | "authorization_suspended"
            | "authorization_revoked",
        ) => "permission_denied",
        Some("background_unavailable" | "background_occluded") => "background_unavailable",
        _ if refused => "refused",
        _ => "tool_error",
    };
    (if refused { "refused" } else { "error" }, class)
}
