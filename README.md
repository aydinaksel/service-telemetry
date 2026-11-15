# Service-Telemetry

Logging and Prometheus metrics shared by my services.

```toml
[dependencies]
service-telemetry = { git = "https://github.com/aydinaksel/service-telemetry", tag = "v0.2.0", features = ["metrics", "otlp"] }
```

## Logging

```rust,ignore
let _logging = service_telemetry::logging::init("info")?;
```

Hold the returned `Logging` until `main` returns: dropping it flushes the logs
still waiting to be exported. `RUST_LOG` replaces the default filter when it is
set. Emit events with `tracing::event!`, a `name:` and a message template, as
M-LOG-STRUCTURED describes.

### Over OTLP

With the `otlp` feature, setting `OTEL_EXPORTER_OTLP_ENDPOINT` (or
`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT`) sends every event as an OTLP/HTTP log
record, and `OTEL_SERVICE_NAME` names the service. Each record carries:

- the message with its placeholders filled in as the body, so
  `EDI transfer is {edi.switch_state}` arrives as `EDI transfer is running`,
- every field as an attribute under its dotted name, keeping its type,
- the event name as `event`, and the template as `message_template`.

Loki stores the body as the log line and the attributes as structured
metadata, so Grafana shows the readable sentence and still filters on
`edi_unfinished_count`. Standard error keeps a plain text copy for the host's
journal. A panic is logged as a `process.panic` error and flushed before the
process unwinds.

### Standard Error Only

Without an OTLP endpoint, events go to standard error alone. On a terminal
each event is colored text. Anywhere else, such as under systemd, each event
is one JSON object per line: the event name under `event`, its fields at the
top level with their dotted names, and the current span's fields under `span`.

## Metrics

```rust,ignore
service_telemetry::metrics::install(metrics_address).await?;

let router = Router::new()
    .route("/orders/{order_number}", get(order))
    .layer(service_telemetry::metrics::RequestMetricsLayer);
```

`install` refuses any address that is not loopback, binds before it returns,
and serves `/metrics` in the background. Until it is called, the `metrics`
macros do nothing.

Add `RequestMetricsLayer` with `Router::layer` after the routes, so it wraps
every route and the fallback. It records `http_server_requests_total` and
`http_server_request_duration_seconds`, labelled by `route` (the matched
template, or `unmatched`) and `status`.

Every histogram is exported with buckets, never as a summary. Names ending in
`_rows` get `ROW_BUCKETS`, everything else `DURATION_BUCKETS_SECONDS`.

### Labels

Label values must come from small fixed sets: a route template, a query name,
a job name, an outcome. Never use an ID, a customer, a path, or free text.
Every distinct value is a new time series that Prometheus keeps.

Never name a label `job` or `instance`. Prometheus sets those on every scraped
series, and renames a clashing label to `exported_job` or `exported_instance`.

### Jobs

```rust,ignore
let run = JobRun::start("floor_and_decor_sync");
let outcome = if import.is_ok() { JobOutcome::Success } else { JobOutcome::Failure };
run.finish(outcome);
```

Each finished run increments `job_runs_total` and records
`job_duration_seconds`, both labelled by `job_name` and `outcome`. A success
also sets `job_last_success_timestamp_seconds{job_name}` to the current Unix
time. At startup, call `record_last_success` with the job's persisted last
success, so a restart does not make the job look like it never succeeded.
