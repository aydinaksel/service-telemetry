mod requests;

use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::routing::get;
use metrics_exporter_prometheus::{
    BuildError, Matcher, PrometheusBuilder, PrometheusHandle, PrometheusRecorder,
};
use tokio::net::TcpListener;
use tracing::{Level, event};

pub use requests::{
    REQUEST_DURATION_SECONDS, REQUESTS_TOTAL, RequestMetrics, RequestMetricsLayer, UNMATCHED_ROUTE,
};

pub const METRICS_PATH: &str = "/metrics";

pub const DURATION_BUCKETS_SECONDS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 300.0, 900.0,
];

pub const ROW_BUCKETS: &[f64] = &[0.0, 1.0, 10.0, 100.0, 1_000.0, 10_000.0, 100_000.0];

const ROW_SUFFIX: &str = "_rows";
const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("metrics address {0} is not a loopback address")]
    NotLoopback(SocketAddr),
    #[error("binding the metrics listener to {address}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
    #[error("configuring the Prometheus recorder")]
    Build(#[from] BuildError),
    #[error("a metrics recorder is already installed")]
    AlreadyInstalled(#[from] ::metrics::SetRecorderError<PrometheusRecorder>),
}

pub async fn install(address: SocketAddr) -> Result<(), InstallError> {
    if !address.ip().is_loopback() {
        return Err(InstallError::NotLoopback(address));
    }
    let recorder = recorder()?;
    let handle = recorder.handle();
    let listener = TcpListener::bind(address)
        .await
        .map_err(|source| InstallError::Bind { address, source })?;
    ::metrics::set_global_recorder(recorder)?;
    tokio::spawn(run_upkeep(handle.clone()));
    tokio::spawn(serve(listener, router(handle)));
    event!(
        name: "metrics.install.success",
        Level::INFO,
        server.address = %address,
        "serving metrics on {{server.address}}",
    );
    Ok(())
}

fn recorder() -> Result<PrometheusRecorder, BuildError> {
    Ok(PrometheusBuilder::new()
        .set_buckets(DURATION_BUCKETS_SECONDS)?
        .set_buckets_for_metric(Matcher::Suffix(ROW_SUFFIX.to_owned()), ROW_BUCKETS)?
        .build_recorder())
}

fn router(handle: PrometheusHandle) -> Router {
    Router::new().route(METRICS_PATH, get(move || async move { handle.render() }))
}

async fn run_upkeep(handle: PrometheusHandle) {
    let mut interval = tokio::time::interval(UPKEEP_INTERVAL);
    loop {
        interval.tick().await;
        handle.run_upkeep();
    }
}

async fn serve(listener: TcpListener, router: Router) {
    if let Err(error) = axum::serve(listener, router).await {
        event!(
            name: "metrics.serve.failure",
            Level::ERROR,
            error.message = %error,
            "metrics endpoint stopped: {{error.message}}",
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;

    const BODY_LIMIT_BYTES: usize = 1 << 20;

    pub(crate) fn test_recorder() -> PrometheusRecorder {
        recorder().unwrap()
    }

    pub(crate) async fn scrape(handle: PrometheusHandle) -> String {
        let response = router(handle)
            .oneshot(Request::get(METRICS_PATH).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), BODY_LIMIT_BYTES)
            .await
            .unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn install_refuses_an_address_beyond_this_host() {
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 9100);

        let result = install(address).await;

        assert!(matches!(result, Err(InstallError::NotLoopback(refused)) if refused == address));
    }

    #[tokio::test]
    async fn durations_and_rows_are_histograms_with_their_own_buckets() {
        let recorder = test_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            ::metrics::histogram!("sap_query_duration_seconds", "query" => "open_orders")
                .record(0.2);
            ::metrics::histogram!("sap_query_rows", "query" => "open_orders").record(42.0);
        });

        let text = scrape(recorder.handle()).await;

        assert!(
            text.contains(r#"sap_query_duration_seconds_bucket{query="open_orders",le="0.25"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(r#"sap_query_rows_bucket{query="open_orders",le="100"} 1"#),
            "{text}"
        );
    }

    #[tokio::test]
    async fn any_histogram_is_exported_with_buckets_never_as_a_summary() {
        let recorder = test_recorder();
        ::metrics::with_local_recorder(&recorder, || {
            ::metrics::histogram!("unsuffixed_histogram").record(1.0);
        });

        let text = scrape(recorder.handle()).await;

        assert!(text.contains("unsuffixed_histogram_bucket"), "{text}");
        assert!(!text.contains("quantile"), "{text}");
    }
}
