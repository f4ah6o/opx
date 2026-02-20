use opentelemetry::{global, KeyValue};
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::{
    trace::{Sampler, SdkTracerProvider},
    Resource,
};
use std::process::Command;

pub struct TelemetryHandle {
    provider: Option<SdkTracerProvider>,
}

impl TelemetryHandle {
    pub fn disabled() -> Self {
        Self { provider: None }
    }

    pub fn shutdown_best_effort(self) {
        if let Some(provider) = self.provider {
            if let Err(err) = provider.shutdown() {
                eprintln!("Warning: telemetry shutdown failed: {err}");
            }
        }
    }
}

pub fn init(command_hint: &str, service_version: &str) -> TelemetryHandle {
    let Some(endpoint) = std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok() else {
        return TelemetryHandle::disabled();
    };

    let service_name =
        std::env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| env!("CARGO_PKG_NAME").to_string());
    let git_commit = resolve_git_commit();
    let sampler = sampler_from_env();

    let exporter = match opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .with_timeout(std::time::Duration::from_millis(1000))
        .build()
    {
        Ok(exporter) => exporter,
        Err(err) => {
            eprintln!(
                "Warning: failed to initialize OTLP exporter for {command_hint}: {err}. Telemetry disabled."
            );
            return TelemetryHandle::disabled();
        }
    };

    let resource = Resource::builder()
        .with_service_name(service_name)
        .with_attribute(KeyValue::new(
            "service.version",
            service_version.to_string(),
        ))
        .with_attribute(KeyValue::new("git.commit", git_commit))
        .build();

    let provider = SdkTracerProvider::builder()
        .with_resource(resource)
        .with_sampler(sampler)
        .with_simple_exporter(exporter)
        .build();

    global::set_tracer_provider(provider.clone());

    TelemetryHandle {
        provider: Some(provider),
    }
}

fn resolve_git_commit() -> String {
    if let Ok(v) = std::env::var("OPZ_GIT_COMMIT") {
        let trimmed = v.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    let out = Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output();

    match out {
        Ok(output) if output.status.success() => {
            let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if commit.is_empty() {
                "unknown".to_string()
            } else {
                commit
            }
        }
        _ => "unknown".to_string(),
    }
}

fn sampler_from_env() -> Sampler {
    let Some(raw) = std::env::var("OTEL_TRACES_SAMPLER").ok() else {
        return Sampler::AlwaysOn;
    };

    let sampler_name = raw.to_ascii_lowercase();
    match sampler_name.as_str() {
        "always_on" => Sampler::AlwaysOn,
        "always_off" => Sampler::AlwaysOff,
        "traceidratio" => Sampler::TraceIdRatioBased(sample_ratio_arg()),
        "parentbased_always_on" => Sampler::ParentBased(Box::new(Sampler::AlwaysOn)),
        "parentbased_always_off" => Sampler::ParentBased(Box::new(Sampler::AlwaysOff)),
        "parentbased_traceidratio" => {
            Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(sample_ratio_arg())))
        }
        _ => {
            eprintln!(
                "Warning: unsupported OTEL_TRACES_SAMPLER={raw}. Falling back to AlwaysOn sampler."
            );
            Sampler::AlwaysOn
        }
    }
}

fn sample_ratio_arg() -> f64 {
    std::env::var("OTEL_TRACES_SAMPLER_ARG")
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .map(|value| value.clamp(0.0, 1.0))
        .unwrap_or(1.0)
}
