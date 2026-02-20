use anyhow::Result;
use opentelemetry::{
    global,
    trace::{Span, TraceContextExt, Tracer},
    Context, KeyValue,
};
use regex::Regex;
use std::ffi::OsString;
use std::process::Command;
use std::sync::OnceLock;

const TRACE_TEXT_LIMIT: usize = 512;

pub fn with_span<T>(name: &str, attrs: Vec<KeyValue>, f: impl FnOnce() -> T) -> T {
    let tracer = global::tracer("opz");
    let mut span = tracer.start_with_context(name.to_string(), &Context::current());
    for attr in attrs {
        span.set_attribute(attr);
    }

    let cx = Context::current_with_span(span);
    let _guard = cx.attach();
    f()
}

pub fn with_span_result<T>(
    name: &str,
    attrs: Vec<KeyValue>,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_span(name, attrs, || {
        let result = f();
        if let Err(err) = &result {
            record_error_message(&err.to_string());
        }
        result
    })
}

pub fn record_error_message(message: &str) {
    let sanitized = sanitize_for_trace(message);
    let cx = Context::current();
    let span = cx.span();
    span.set_status(opentelemetry::trace::Status::error(sanitized.clone()));
    span.add_event(
        "exception".to_string(),
        vec![KeyValue::new("exception.message", sanitized)],
    );
}

pub fn build_cli_trace_attrs(command_name: &str, args: &[OsString]) -> Vec<KeyValue> {
    let mut attrs = vec![
        KeyValue::new("cli.command", command_name.to_string()),
        KeyValue::new("cli.args_count", (args.len().saturating_sub(1)) as i64),
        KeyValue::new("git.commit", resolve_git_commit_attr()),
    ];

    if let Ok(cwd) = std::env::current_dir() {
        attrs.push(KeyValue::new("cwd", cwd.display().to_string()));
    }

    if std::env::var("OPZ_TRACE_CAPTURE_ARGS").ok().as_deref() == Some("1") {
        let raw_args = args
            .iter()
            .skip(1)
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        attrs.push(KeyValue::new("cli.args", sanitize_for_trace(&raw_args)));
    }

    attrs
}

fn resolve_git_commit_attr() -> String {
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
            let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if value.is_empty() {
                "unknown".to_string()
            } else {
                value
            }
        }
        _ => "unknown".to_string(),
    }
}

pub fn sanitize_for_trace(input: &str) -> String {
    let masked_op = op_reference_regex().replace_all(input, "op://***");
    let masked_keys = secret_key_value_regex().replace_all(&masked_op, "$1***");

    let mut out = masked_keys.into_owned();
    if out.len() > TRACE_TEXT_LIMIT {
        out.truncate(TRACE_TEXT_LIMIT);
        out.push_str("...[truncated]");
    }

    out
}

fn op_reference_regex() -> &'static Regex {
    static OP_REFERENCE_REGEX: OnceLock<Regex> = OnceLock::new();
    OP_REFERENCE_REGEX.get_or_init(|| Regex::new(r#"op://[^\s"']+"#).expect("valid op ref regex"))
}

fn secret_key_value_regex() -> &'static Regex {
    static SECRET_KEY_VALUE_REGEX: OnceLock<Regex> = OnceLock::new();
    SECRET_KEY_VALUE_REGEX.get_or_init(|| {
        Regex::new(
            r"(?i)((?:^|[?&\s,;])(?:token|password|passwd|secret|apikey|api_key|access_key|client_secret)=)[^\s&]+",
        )
        .expect("valid secret key regex")
    })
}

#[cfg(test)]
mod tests {
    use super::sanitize_for_trace;

    #[test]
    fn test_sanitize_for_trace_masks_op_reference() {
        let sanitized = sanitize_for_trace("read op://vault/item/field now");
        assert_eq!(sanitized, "read op://*** now");
    }

    #[test]
    fn test_sanitize_for_trace_masks_secret_key_values() {
        let sanitized = sanitize_for_trace("token=abc123 password=p@ssw0rd");
        assert_eq!(sanitized, "token=*** password=***");
    }

    #[test]
    fn test_sanitize_for_trace_masks_query_like_tokens() {
        let sanitized = sanitize_for_trace("https://x.test/api?api_key=abc&foo=bar");
        assert_eq!(sanitized, "https://x.test/api?api_key=***&foo=bar");
    }

    #[test]
    fn test_sanitize_for_trace_truncates_long_text() {
        let long = "a".repeat(600);
        let sanitized = sanitize_for_trace(&long);
        assert!(sanitized.ends_with("...[truncated]"));
        assert!(sanitized.len() > 512);
    }
}
