//! A tiny web server whose index page randomly times out, for practicing debugging slow requests.
//!
//! `GET /` answers 200 right away, except for a random share of requests (TIMEOUT_RATE) that hang for
//! TIMEOUT_SECONDS and then answer 504 Gateway Timeout - a client with a shorter timeout gives up first.
//! `GET /healthz` always answers 200, for liveness/readiness probes.
//! `GET /metrics` serves Prometheus metrics: request count, latency histogram and in-flight requests per route.
//!
//!     cargo run                                   # :8080, 20% of requests hang for 30s
//!     PORT=3000 TIMEOUT_RATE=0.5 TIMEOUT_SECONDS=5 cargo run

use std::{env, sync::Arc, time::Duration};

use axum::{
    Router,
    extract::{MatchedPath, Request, State},
    http::{StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use prometheus::{
    Encoder, HistogramOpts, HistogramVec, IntCounterVec, IntGaugeVec, Opts, Registry, TextEncoder,
};
use tokio::time::{Instant, sleep};

struct Config {
    timeout_rate: f64,
    timeout: Duration,
}

struct Metrics {
    registry: Registry,
    requests: IntCounterVec,
    duration: HistogramVec,
    in_flight: IntGaugeVec,
}

impl Metrics {
    fn new() -> Self {
        let requests = IntCounterVec::new(
            Opts::new("http_requests_total", "HTTP requests served"),
            &["method", "path", "status"],
        )
        .unwrap();
        // Buckets reach past the 30s default hang, so timed-out requests land in their own bucket.
        let duration = HistogramVec::new(
            HistogramOpts::new("http_request_duration_seconds", "HTTP request latency").buckets(vec![
                0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
            ]),
            &["method", "path", "status"],
        )
        .unwrap();
        let in_flight = IntGaugeVec::new(
            Opts::new("http_requests_in_flight", "HTTP requests currently being served"),
            &["path"],
        )
        .unwrap();
        let registry = Registry::new();
        registry.register(Box::new(requests.clone())).unwrap();
        registry.register(Box::new(duration.clone())).unwrap();
        registry.register(Box::new(in_flight.clone())).unwrap();
        Metrics { registry, requests, duration, in_flight }
    }
}

/// Count and time every request, labelled by route (not raw path, to keep label cardinality bounded).
async fn track(State(metrics): State<Arc<Metrics>>, request: Request, next: Next) -> Response {
    let path = match request.extensions().get::<MatchedPath>() {
        Some(matched) => matched.as_str().to_owned(),
        None => "unmatched".to_owned(),
    };
    let method = request.method().to_string();
    let in_flight = metrics.in_flight.with_label_values(&[&path]);
    in_flight.inc();
    let start = Instant::now();
    let response = next.run(request).await;
    in_flight.dec();
    let status = response.status().as_u16().to_string();
    let labels = [method.as_str(), path.as_str(), status.as_str()];
    metrics.requests.with_label_values(&labels).inc();
    metrics.duration.with_label_values(&labels).observe(start.elapsed().as_secs_f64());
    response
}

async fn metrics_handler(State(metrics): State<Arc<Metrics>>) -> impl IntoResponse {
    let mut body = Vec::new();
    TextEncoder::new().encode(&metrics.registry.gather(), &mut body).unwrap();
    ([(header::CONTENT_TYPE, TextEncoder::new().format_type().to_owned())], body)
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

async fn index(State(config): State<Arc<Config>>) -> (StatusCode, &'static str) {
    let start = Instant::now();
    if fastrand::f64() < config.timeout_rate {
        sleep(config.timeout).await;
        println!("GET / 504 {}ms (timed out waiting for upstream)", start.elapsed().as_millis());
        return (StatusCode::GATEWAY_TIMEOUT, "upstream timed out\n");
    }
    println!("GET / 200 {}ms", start.elapsed().as_millis());
    (StatusCode::OK, "hello from flaky-web\n")
}

#[tokio::main]
async fn main() {
    let port: u16 = env_or("PORT", 8080);
    let config = Arc::new(Config {
        timeout_rate: env_or("TIMEOUT_RATE", 0.2),
        timeout: Duration::from_secs_f64(env_or("TIMEOUT_SECONDS", 30.0)),
    });
    println!(
        "flaky-web listening on :{port} (timeout_rate={}, timeout={:?})",
        config.timeout_rate, config.timeout
    );

    let metrics = Arc::new(Metrics::new());

    let app = Router::new()
        .route("/", get(index))
        .route("/healthz", get(|| async { "ok\n" }))
        .with_state(config)
        .route("/metrics", get(metrics_handler).with_state(metrics.clone()))
        .route_layer(middleware::from_fn_with_state(metrics, track));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

/// Ctrl-C locally, SIGTERM from Kubernetes.
async fn shutdown_signal() {
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}
