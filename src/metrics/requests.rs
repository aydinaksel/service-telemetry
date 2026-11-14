use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use axum::extract::MatchedPath;
use axum::http::{Request, Response};
use tower::{Layer, Service};

pub const REQUESTS_TOTAL: &str = "http_server_requests_total";
pub const REQUEST_DURATION_SECONDS: &str = "http_server_request_duration_seconds";
pub const UNMATCHED_ROUTE: &str = "unmatched";

const ROUTE_LABEL: &str = "route";
const STATUS_LABEL: &str = "status";

#[derive(Clone, Copy, Debug, Default)]
pub struct RequestMetricsLayer;

impl<S> Layer<S> for RequestMetricsLayer {
    type Service = RequestMetrics<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequestMetrics { inner }
    }
}

#[derive(Clone, Debug)]
pub struct RequestMetrics<S> {
    inner: S,
}

type ResponseFuture<Output> = Pin<Box<dyn Future<Output = Output> + Send>>;

impl<S, RequestBody, ResponseBody> Service<Request<RequestBody>> for RequestMetrics<S>
where
    S: Service<Request<RequestBody>, Response = Response<ResponseBody>>,
    S::Future: Send + 'static,
    S::Error: 'static,
    ResponseBody: 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = ResponseFuture<Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, context: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(context)
    }

    fn call(&mut self, request: Request<RequestBody>) -> Self::Future {
        let route = request
            .extensions()
            .get::<MatchedPath>()
            .map_or(UNMATCHED_ROUTE, MatchedPath::as_str)
            .to_owned();
        let started_at = Instant::now();
        let response = self.inner.call(request);
        Box::pin(async move {
            let response = response.await?;
            let status = response.status().as_u16().to_string();
            let labels = [(ROUTE_LABEL, route), (STATUS_LABEL, status)];
            ::metrics::counter!(REQUESTS_TOTAL, &labels).increment(1);
            ::metrics::histogram!(REQUEST_DURATION_SECONDS, &labels).record(started_at.elapsed());
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::StatusCode;
    use axum::routing::get;
    use tower::ServiceExt;

    use super::*;
    use crate::metrics::tests::{scrape, test_recorder};

    fn orders_router() -> Router {
        Router::new()
            .route("/orders/{order_number}", get(|| async { "order" }))
            .route(
                "/broken",
                get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .layer(RequestMetricsLayer)
    }

    async fn send(router: Router, path: &str) -> StatusCode {
        router
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn requests_are_labelled_by_route_template_and_status() {
        let recorder = test_recorder();
        let _guard = ::metrics::set_default_local_recorder(&recorder);

        send(orders_router(), "/orders/1001").await;
        send(orders_router(), "/orders/1002").await;
        send(orders_router(), "/broken").await;

        let text = scrape(recorder.handle()).await;
        assert!(
            text.contains(
                r#"http_server_requests_total{route="/orders/{order_number}",status="200"} 2"#
            ),
            "{text}"
        );
        assert!(
            text.contains(r#"http_server_requests_total{route="/broken",status="500"} 1"#),
            "{text}"
        );
        assert!(
            text.contains(
                r#"http_server_request_duration_seconds_count{route="/orders/{order_number}",status="200"} 2"#
            ),
            "{text}"
        );
        assert!(!text.contains("1001"), "{text}");
    }

    #[tokio::test]
    async fn unmatched_paths_share_one_label_value() {
        let recorder = test_recorder();
        let _guard = ::metrics::set_default_local_recorder(&recorder);

        let status = send(orders_router(), "/wp-login.php").await;

        assert_eq!(status, StatusCode::NOT_FOUND);
        let text = scrape(recorder.handle()).await;
        assert!(
            text.contains(r#"http_server_requests_total{route="unmatched",status="404"} 1"#),
            "{text}"
        );
        assert!(!text.contains("wp-login"), "{text}");
    }
}
