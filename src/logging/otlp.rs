use std::io;
use std::panic::{self, PanicHookInfo};
use std::time::SystemTime;

use opentelemetry::logs::{AnyValue, LogRecord, Logger, LoggerProvider, Severity};
use opentelemetry_otlp::LogExporter;
use opentelemetry_sdk::logs::{SdkLogger, SdkLoggerProvider};
use tracing::{Event, Level, Subscriber, event};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::filter_fn;
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;

use super::fields::EventFields;
use super::{EVENT_NAME_KEY, Format, InitError, Logging, layer};

const ENDPOINT_VARIABLES: [&str; 2] = [
    "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT",
    "OTEL_EXPORTER_OTLP_ENDPOINT",
];
const INSTRUMENTATION_SCOPE: &str = "service-telemetry";
const MESSAGE_TEMPLATE_KEY: &str = "message_template";
const EXPORTER_TARGET_PREFIXES: [&str; 5] = ["opentelemetry", "hyper", "reqwest", "h2", "tower"];
const UNKNOWN_PANIC_MESSAGE: &str = "a panic without a message";

#[derive(Debug)]
pub(super) struct Export {
    provider: SdkLoggerProvider,
}

impl Drop for Export {
    fn drop(&mut self) {
        if let Err(error) = self.provider.shutdown() {
            event!(
                name: "logging.otlp.shutdown.failure",
                Level::WARN,
                error.message = %error,
                "flushing the last logs over OTLP failed: {{error.message}}",
            );
        }
    }
}

pub(super) fn endpoint_configured() -> bool {
    ENDPOINT_VARIABLES
        .iter()
        .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
}

pub(super) fn init(
    filter: EnvFilter,
    standard_error_is_terminal: bool,
) -> Result<Logging, InitError> {
    let standard_error = layer(
        Format::Text {
            colored: standard_error_is_terminal,
        },
        io::stderr,
    );
    let exporter = match LogExporter::builder().with_http().build() {
        Ok(exporter) => exporter,
        Err(error) => {
            tracing_subscriber::registry()
                .with(filter)
                .with(standard_error)
                .try_init()?;
            return Err(InitError::Exporter(error));
        }
    };
    let provider = SdkLoggerProvider::builder()
        .with_batch_exporter(exporter)
        .build();
    tracing_subscriber::registry()
        .with(filter)
        .with(standard_error)
        .with(otlp_layer(&provider))
        .try_init()?;
    install_panic_hook(provider.clone());
    Ok(Logging {
        export: Some(Export { provider }),
    })
}

fn is_exporter_internal(target: &str) -> bool {
    EXPORTER_TARGET_PREFIXES
        .iter()
        .any(|prefix| target.starts_with(prefix))
}

struct OtlpLayer {
    logger: SdkLogger,
}

fn otlp_layer<S>(provider: &SdkLoggerProvider) -> impl Layer<S>
where
    S: Subscriber + for<'span> LookupSpan<'span>,
{
    OtlpLayer {
        logger: provider.logger(INSTRUMENTATION_SCOPE),
    }
    .with_filter(filter_fn(|metadata| {
        !is_exporter_internal(metadata.target())
    }))
}

impl<S> Layer<S> for OtlpLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let mut fields = EventFields::default();
        event.record(&mut fields);

        let mut record = self.logger.create_log_record();
        record.set_timestamp(SystemTime::now());
        record.set_event_name(metadata.name());
        record.set_target(metadata.target());
        record.set_severity_number(severity(*metadata.level()));
        record.set_severity_text(metadata.level().as_str());
        record.set_body(AnyValue::from(fields.rendered_message()));
        record.add_attribute(EVENT_NAME_KEY, metadata.name());
        if let Some(template) = fields.template {
            record.add_attribute(MESSAGE_TEMPLATE_KEY, template);
        }
        for (name, value) in fields.values {
            record.add_attribute(name, AnyValue::from(value));
        }
        self.logger.emit(record);
    }
}

fn severity(level: Level) -> Severity {
    match level {
        Level::ERROR => Severity::Error,
        Level::WARN => Severity::Warn,
        Level::INFO => Severity::Info,
        Level::DEBUG => Severity::Debug,
        _ => Severity::Trace,
    }
}

fn install_panic_hook(provider: SdkLoggerProvider) {
    let previous_hook = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        event!(
            name: "process.panic",
            Level::ERROR,
            panic.message = panic_message(panic_info),
            code.location = panic_info.location().map(ToString::to_string).unwrap_or_default(),
            "panicked at {{code.location}}: {{panic.message}}",
        );
        provider.force_flush().unwrap_or_default();
        previous_hook(panic_info);
    }));
}

fn panic_message(panic_info: &PanicHookInfo<'_>) -> String {
    let payload = panic_info.payload();
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| UNKNOWN_PANIC_MESSAGE.to_owned())
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use opentelemetry::Key;
    use opentelemetry_otlp::WithExportConfig;
    use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLogRecord};
    use tracing::Level;

    use super::*;

    fn export_to_memory(emit: impl FnOnce()) -> Vec<SdkLogRecord> {
        let exporter = InMemoryLogExporter::default();
        let provider = SdkLoggerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let subscriber = tracing_subscriber::registry().with(otlp_layer(&provider));
        tracing::subscriber::with_default(subscriber, emit);
        exporter
            .get_emitted_logs()
            .unwrap()
            .into_iter()
            .map(|log| log.record)
            .collect()
    }

    fn attribute<'record>(record: &'record SdkLogRecord, key: &str) -> Option<&'record AnyValue> {
        record
            .attributes_iter()
            .find(|(attribute_key, _)| *attribute_key == Key::from(key.to_owned()))
            .map(|(_, value)| value)
    }

    fn emit_transfer_heartbeat() {
        event!(
            name: "edi.transfer.heartbeat",
            Level::INFO,
            edi.unfinished_count = 3,
            edi.switch_state = "running",
            "EDI transfer is {{edi.switch_state}}, {{edi.unfinished_count}} in flight",
        );
    }

    #[test]
    fn the_body_is_the_rendered_message() {
        let records = export_to_memory(emit_transfer_heartbeat);

        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].body(),
            Some(&AnyValue::from(
                "EDI transfer is running, 3 in flight".to_owned()
            ))
        );
    }

    #[test]
    fn fields_become_typed_attributes_under_their_dotted_names() {
        let records = export_to_memory(emit_transfer_heartbeat);
        let record = &records[0];

        assert_eq!(
            attribute(record, "edi.unfinished_count"),
            Some(&AnyValue::Int(3))
        );
        assert_eq!(
            attribute(record, "edi.switch_state"),
            Some(&AnyValue::from("running".to_owned()))
        );
        assert_eq!(
            attribute(record, "event"),
            Some(&AnyValue::from("edi.transfer.heartbeat"))
        );
        assert_eq!(
            attribute(record, "message_template"),
            Some(&AnyValue::from(
                "EDI transfer is {edi.switch_state}, {edi.unfinished_count} in flight".to_owned()
            ))
        );
        assert!(attribute(record, "message").is_none());
    }

    #[test]
    fn the_record_carries_severity_and_event_name() {
        let records = export_to_memory(|| {
            event!(name: "sap.query.failure", Level::ERROR, "the query failed");
        });
        let record = &records[0];

        assert_eq!(record.severity_number(), Some(Severity::Error));
        assert_eq!(record.severity_text(), Some("ERROR"));
        assert_eq!(record.event_name(), Some("sap.query.failure"));
    }

    #[test]
    fn the_exporter_does_not_export_its_own_logs() {
        assert!(is_exporter_internal("opentelemetry_sdk::logs"));
        assert!(is_exporter_internal("hyper_util::client"));
        assert!(is_exporter_internal("reqwest::blocking"));
        assert!(!is_exporter_internal("edi_transfer::service"));
        assert!(!is_exporter_internal("sap_database::client"));
    }

    fn capture_one_request(listener: TcpListener) -> thread::JoinHandle<Vec<u8>> {
        thread::spawn(move || {
            let (mut connection, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(connection.try_clone().unwrap());
            let mut request = Vec::new();
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                request.extend_from_slice(line.as_bytes());
                if let Some(length) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = length.trim().parse().unwrap();
                }
                if line == "\r\n" {
                    break;
                }
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            request.extend_from_slice(&body);
            connection
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: application/x-protobuf\r\ncontent-length: 0\r\n\r\n")
                .unwrap();
            request
        })
    }

    #[tokio::test]
    async fn logs_are_exported_over_http_from_inside_a_tokio_runtime() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1/logs", listener.local_addr().unwrap());
        let request = capture_one_request(listener);
        let exporter = LogExporter::builder()
            .with_http()
            .with_endpoint(endpoint)
            .build()
            .unwrap();
        let provider = SdkLoggerProvider::builder()
            .with_batch_exporter(exporter)
            .build();
        let subscriber = tracing_subscriber::registry().with(otlp_layer(&provider));
        tracing::subscriber::with_default(subscriber, emit_transfer_heartbeat);

        provider.shutdown().unwrap();

        let request = String::from_utf8_lossy(&request.join().unwrap()).into_owned();
        assert!(request.starts_with("POST /v1/logs "), "{request}");
        assert!(
            request.contains("EDI transfer is running, 3 in flight"),
            "{request}"
        );
        assert!(request.contains("edi.unfinished_count"), "{request}");
    }
}
