//! Tracing / JSON logging initialisation (design.md §9).
//!
//! Production logs are structured JSON lines on stdout; a human-readable
//! "pretty" format is available for interactive use. The [`EnvFilter`] is
//! derived from `RUST_LOG`, falling back to [`DEFAULT_DIRECTIVE`].

use std::fmt;

use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt as fmt_layer, EnvFilter};

/// Directive used when `RUST_LOG` is unset or blank.
pub const DEFAULT_DIRECTIVE: &str = "info";

/// The output format for the tracing subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// Newline-delimited structured JSON (production default).
    Json,
    /// Human-readable, multi-line output for local development.
    Pretty,
}

/// Error returned by [`init`] when the global subscriber is already set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlreadyInitialized;

impl fmt::Display for AlreadyInitialized {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("global tracing subscriber was already initialised")
    }
}

impl std::error::Error for AlreadyInitialized {}

/// Map the `json` flag to a [`LogFormat`].
///
/// P0 always runs JSON in production; the flag keeps the choice explicit and
/// testable instead of hard-coding it inside [`init`].
pub fn log_format(json: bool) -> LogFormat {
    if json {
        LogFormat::Json
    } else {
        LogFormat::Pretty
    }
}

/// Resolve the effective filter directive from a raw `RUST_LOG` value.
///
/// Blank/whitespace-only values fall back to [`DEFAULT_DIRECTIVE`]; otherwise
/// the value is trimmed of surrounding whitespace.
pub fn effective_directive(raw: Option<&str>) -> String {
    match raw.map(str::trim).filter(|d| !d.is_empty()) {
        Some(directive) => directive.to_owned(),
        None => DEFAULT_DIRECTIVE.to_owned(),
    }
}

/// Build an [`EnvFilter`] from a raw `RUST_LOG` value.
///
/// Parsing is lossy: invalid directives are reported by `tracing` rather than
/// aborting startup, so a typo in `RUST_LOG` never takes the process down.
pub fn build_env_filter(raw: Option<&str>) -> EnvFilter {
    EnvFilter::builder()
        .with_default_directive(LevelFilter::INFO.into())
        .parse_lossy(effective_directive(raw).as_str())
}

/// Install the global tracing subscriber.
///
/// Returns [`AlreadyInitialized`] if a subscriber was already installed in this
/// process (the caller decides whether that is fatal).
pub fn init(json: bool) -> Result<(), AlreadyInitialized> {
    let filter = build_env_filter(std::env::var("RUST_LOG").ok().as_deref());
    let subscriber = tracing_subscriber::registry().with(filter);
    match log_format(json) {
        LogFormat::Json => subscriber.with(fmt_layer::layer().json()).try_init(),
        LogFormat::Pretty => subscriber.with(fmt_layer::layer()).try_init(),
    }
    .map_err(|_| AlreadyInitialized)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_format_maps_flag() {
        assert_eq!(log_format(true), LogFormat::Json);
        assert_eq!(log_format(false), LogFormat::Pretty);
    }

    #[test]
    fn effective_directive_defaults_when_absent_or_blank() {
        assert_eq!(effective_directive(None), "info");
        assert_eq!(effective_directive(Some("")), "info");
        assert_eq!(effective_directive(Some("   ")), "info");
        assert_eq!(effective_directive(Some("\t\n")), "info");
    }

    #[test]
    fn effective_directive_trims_explicit_value() {
        assert_eq!(effective_directive(Some(" iris=debug ")), "iris=debug");
        assert_eq!(
            effective_directive(Some("warn,iris_http=trace")),
            "warn,iris_http=trace"
        );
    }

    #[test]
    fn build_env_filter_tolerates_valid_and_invalid_directives() {
        // Must not panic for either valid or malformed input.
        let _ = build_env_filter(None);
        let _ = build_env_filter(Some("iris=debug,pingora=info"));
        let _ = build_env_filter(Some("this is not a valid directive"));
    }
}
