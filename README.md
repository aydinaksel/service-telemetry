# Service-Telemetry

Logging and Prometheus metrics shared by my services.

```toml
[dependencies]
service-telemetry = { git = "https://github.com/aydinaksel/service-telemetry", tag = "v0.1.0", features = ["metrics"] }
```

## Logging

```rust,ignore
service_telemetry::logging::init("info")?;
```

Events go to standard error. `RUST_LOG` replaces the default filter when it is
set. On a terminal each event is colored text. Anywhere else, such as under
systemd, each event is one JSON object per line: the event name under `event`,
its fields at the top level with their dotted names, and the current span's
fields under `span`. Loki parses the line at query time, so
`edi.unfinished_count` becomes `edi_unfinished_count` in Grafana.

Emit events with `tracing::event!`, a `name:` and a message template, as
M-LOG-STRUCTURED describes.

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
