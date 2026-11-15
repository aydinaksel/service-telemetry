#[cfg(feature = "otlp")]
mod fields;
#[cfg(feature = "otlp")]
mod otlp;

use std::fmt;
use std::io::{self, IsTerminal};

use tracing::{Event, Subscriber};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::{self, FormatEvent, FormatFields, Json, JsonFields, Writer};
use tracing_subscriber::fmt::{FmtContext, MakeWriter};
use tracing_subscriber::layer::{Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::{SubscriberInitExt, TryInitError};

const EVENT_NAME_KEY: &str = "event";

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("installing the tracing subscriber: {0}")]
    Subscriber(#[from] TryInitError),
    #[cfg(feature = "otlp")]
    #[error("building the OTLP log exporter, logging to standard error only: {0}")]
    Exporter(#[from] opentelemetry_otlp::ExporterBuildError),
}

#[derive(Debug)]
#[must_use = "dropping Logging stops exporting logs, so hold it until main returns"]
pub struct Logging {
    #[cfg(feature = "otlp")]
    #[cfg_attr(
        feature = "otlp",
        expect(dead_code, reason = "held so that dropping Logging flushes the export")
    )]
    export: Option<otlp::Export>,
}

impl Logging {
    fn standard_error_only() -> Self {
        Self {
            #[cfg(feature = "otlp")]
            export: None,
        }
    }
}

pub fn init(default_filter: &str) -> Result<Logging, InitError> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    let standard_error_is_terminal = io::stderr().is_terminal();
    #[cfg(feature = "otlp")]
    if otlp::endpoint_configured() {
        return otlp::init(filter, standard_error_is_terminal);
    }
    let format = if standard_error_is_terminal {
        Format::Text { colored: true }
    } else {
        Format::Json
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(layer(format, io::stderr))
        .try_init()?;
    Ok(Logging::standard_error_only())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Text { colored: bool },
    Json,
}

fn layer<S, W>(format: Format, writer: W) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
{
    match format {
        Format::Text { colored } => tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .with_ansi(colored)
            .boxed(),
        Format::Json => tracing_subscriber::fmt::layer()
            .with_writer(writer)
            .fmt_fields(JsonFields::new())
            .event_format(NamedJson::default())
            .boxed(),
    }
}

struct NamedJson {
    json: format::Format<Json>,
}

impl Default for NamedJson {
    fn default() -> Self {
        Self {
            json: format::format()
                .json()
                .flatten_event(true)
                .with_span_list(false),
        }
    }
}

impl<S, N> FormatEvent<S, N> for NamedJson
where
    S: Subscriber + for<'span> LookupSpan<'span>,
    N: for<'writer> FormatFields<'writer> + 'static,
{
    fn format_event(
        &self,
        context: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let mut line = String::new();
        self.json
            .format_event(context, Writer::new(&mut line), event)?;
        let Some(remaining_entries) = line.strip_prefix('{') else {
            return writer.write_str(&line);
        };
        let event_name = serde_json::Value::from(event.metadata().name());
        write!(
            writer,
            "{{\"{EVENT_NAME_KEY}\":{event_name},{remaining_entries}"
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, PoisonError};

    use tracing::{Level, event};

    use super::*;

    #[derive(Clone, Default)]
    struct CapturedOutput(Arc<Mutex<Vec<u8>>>);

    impl CapturedOutput {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl io::Write for CapturedOutput {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for CapturedOutput {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    fn capture(format: Format, emit: impl FnOnce()) -> String {
        let output = CapturedOutput::default();
        let subscriber = tracing_subscriber::registry().with(layer(format, output.clone()));
        tracing::subscriber::with_default(subscriber, emit);
        output.text()
    }

    fn emit_transfer_heartbeat() {
        event!(
            name: "edi.transfer.heartbeat",
            Level::INFO,
            edi.unfinished_count = 3,
            edi.switch_state = "on",
            "EDI transfer is {{edi.switch_state}}, {{edi.unfinished_count}} in flight",
        );
    }

    fn parse_single_line(output: &str) -> serde_json::Value {
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1, "expected one line, got {output:?}");
        serde_json::from_str(lines[0]).unwrap()
    }

    #[test]
    fn json_line_carries_the_event_name_and_flattened_fields() {
        let line = parse_single_line(&capture(Format::Json, emit_transfer_heartbeat));

        assert_eq!(line["event"], "edi.transfer.heartbeat");
        assert_eq!(line["level"], "INFO");
        assert_eq!(line["edi.unfinished_count"], 3);
        assert_eq!(line["edi.switch_state"], "on");
        assert_eq!(
            line["message"],
            "EDI transfer is {edi.switch_state}, {edi.unfinished_count} in flight",
        );
        assert!(line.get("fields").is_none());
    }

    #[test]
    fn json_line_carries_the_current_span_fields() {
        let output = capture(Format::Json, || {
            let span = tracing::span!(Level::INFO, "job", job.name = "nightly-report");
            let _entered = span.enter();
            emit_transfer_heartbeat();
        });
        let line = parse_single_line(&output);

        assert_eq!(line["span"]["job.name"], "nightly-report");
        assert!(line.get("spans").is_none());
    }

    #[test]
    fn json_line_escapes_the_event_name() {
        let output = capture(Format::Json, || {
            event!(name: "quote\"in.name", Level::WARN, "named oddly");
        });

        assert_eq!(parse_single_line(&output)["event"], "quote\"in.name");
    }

    #[test]
    fn json_line_contains_no_terminal_color() {
        let output = capture(Format::Json, emit_transfer_heartbeat);

        assert!(!output.contains('\u{1b}'));
    }

    #[test]
    fn text_line_is_colored_and_keeps_fields() {
        let output = capture(Format::Text { colored: true }, emit_transfer_heartbeat);

        assert!(
            output.contains('\u{1b}'),
            "expected ANSI color in {output:?}"
        );
        assert!(output.contains("edi.unfinished_count"));
    }

    #[test]
    fn second_init_returns_an_error_instead_of_panicking() {
        let _logging = init("info").unwrap();

        init("info").unwrap_err();
    }
}
