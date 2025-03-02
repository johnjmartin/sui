// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::config::{LoggingConfig, PeerConfig};
use crate::metrics::AppMetrics;
use axum::{
    body::Body,
    extract::{Request, State},
    http::request::Parts,
    http::StatusCode,
    response::Response,
};
use bytes::Bytes;
use rand::Rng;
use std::time::Instant;
use tracing::{debug, warn};

#[derive(Debug)]
enum PeerRole {
    Read,
    Execution,
}

impl PeerRole {
    fn as_str(&self) -> &'static str {
        match self {
            PeerRole::Read => "read",
            PeerRole::Execution => "execution",
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    client: reqwest::Client,
    read_peer: PeerConfig,
    execution_peer: PeerConfig,
    metrics: AppMetrics,
    logging_config: LoggingConfig,
}

impl AppState {
    pub fn new(
        client: reqwest::Client,
        read_peer: PeerConfig,
        execution_peer: PeerConfig,
        metrics: AppMetrics,
        logging_config: LoggingConfig,
    ) -> Self {
        Self {
            client,
            read_peer,
            execution_peer,
            metrics,
            logging_config,
        }
    }
}

pub async fn proxy_handler(
    State(state): State<AppState>,
    request: Request<Body>,
) -> Result<Response, (StatusCode, String)> {
    let (parts, body) = request.into_parts();
    let body_bytes = match axum::body::to_bytes(body, 10 * 1024 * 1024).await {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!("Failed to read request body: {}", e);
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to read request body: {}", e),
            ));
        }
    };

    let peer_role = determine_peer_role(&parts, &body_bytes);
    proxy_request(state, parts, body_bytes, peer_role).await
}

fn determine_peer_role(parts: &Parts, body_bytes: &Bytes) -> PeerRole {
    if let Some("sui_executeTransactionBlock") = parts
        .headers
        .get("Client-Request-Method")
        .and_then(|h| h.to_str().ok())
    {
        debug!("Using execution peer based on header");
        return PeerRole::Execution;
    }

    if let Ok(json_body) = serde_json::from_slice::<serde_json::Value>(body_bytes) {
        if let Some("sui_executeTransactionBlock") =
            json_body.get("method").and_then(|m| m.as_str())
        {
            debug!("Using execution peer based on JSON body");
            return PeerRole::Execution;
        }
    }

    debug!("Using read peer as fallback");
    PeerRole::Read
}

async fn proxy_request(
    state: AppState,
    parts: Parts,
    body_bytes: Bytes,
    peer_role: PeerRole,
) -> Result<Response, (StatusCode, String)> {
    let peer_type_str = peer_role.as_str();
    debug!(
        "Proxying request: method={:?}, uri={:?}, headers={:?}, body_len={}, peer_type={:?}",
        parts.method,
        parts.uri,
        parts.headers,
        body_bytes.len(),
        peer_role
    );

    maybe_log_sampled_request(&state, &parts, &body_bytes, &peer_role);

    // Record metrics
    let metrics = &state.metrics;
    let timer_histogram = metrics.request_latency.with_label_values(&[peer_type_str]);
    let _timer = timer_histogram.start_timer();
    metrics
        .request_size_bytes
        .with_label_values(&[peer_type_str])
        .observe(body_bytes.len() as f64);

    // Build and send request
    let peer_config = match peer_role {
        PeerRole::Read => &state.read_peer,
        PeerRole::Execution => &state.execution_peer,
    };

    let upstream_response = send_upstream_request(
        &state.client,
        &peer_config,
        &parts,
        body_bytes.clone(),
        peer_type_str,
        &metrics,
    )
    .await?;

    // Process response
    process_upstream_response(upstream_response, peer_type_str, &metrics).await
}

fn maybe_log_sampled_request(
    state: &AppState,
    parts: &Parts,
    body_bytes: &Bytes,
    peer_role: &PeerRole,
) {
    if !matches!(peer_role, PeerRole::Read) {
        return;
    }

    let user_agent = parts
        .headers
        .get("user-agent")
        .and_then(|h| h.to_str().ok());

    let is_health_check = matches!(user_agent, Some(ua) if ua.contains("GoogleHC/1.0"));
    let is_grafana_agent = matches!(user_agent, Some(ua) if ua.contains("GrafanaAgent"));
    let is_grpc = parts
        .headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .map(|ct| ct.contains("grpc"))
        .unwrap_or(false);

    let should_sample = !is_health_check && !is_grafana_agent && !is_grpc;
    let rate = state.logging_config.read_request_sample_rate;

    if should_sample && rand::thread_rng().gen::<f64>() < rate {
        tracing::info!(
            headers = ?parts.headers,
            body = ?body_bytes,
            peer_type = ?peer_role,
            "Sampled read request"
        );
    }
}

async fn send_upstream_request(
    client: &reqwest::Client,
    peer_config: &PeerConfig,
    parts: &Parts,
    body_bytes: Bytes,
    peer_type_str: &str,
    metrics: &AppMetrics,
) -> Result<reqwest::Response, (StatusCode, String)> {
    let mut target_url = peer_config.address.clone();
    target_url.set_path(parts.uri.path());
    if let Some(query) = parts.uri.query() {
        target_url.set_query(Some(query));
    }

    // remove host header to avoid interfering with reqwest auto-host header
    let mut headers = parts.headers.clone();
    headers.remove("host");

    let request_builder = client
        .request(parts.method.clone(), target_url)
        .headers(headers)
        .body(body_bytes);

    debug!("Request builder: {:?}", request_builder);

    let upstream_start = std::time::Instant::now();
    match request_builder.send().await {
        Ok(response) => {
            let status = response.status().as_u16().to_string();
            metrics
                .upstream_response_latency
                .with_label_values(&[peer_type_str, &status])
                .observe(upstream_start.elapsed().as_secs_f64());
            metrics
                .requests_total
                .with_label_values(&[peer_type_str, &status])
                .inc();
            debug!("Response: {:?}", response);
            Ok(response)
        }
        Err(e) => {
            warn!("Failed to send request: {}", e);
            metrics
                .upstream_response_latency
                .with_label_values(&[peer_type_str, "error"])
                .observe(upstream_start.elapsed().as_secs_f64());
            metrics
                .requests_total
                .with_label_values(&[peer_type_str, "error"])
                .inc();
            if e.is_timeout() {
                metrics
                    .timeouts_total
                    .with_label_values(&[peer_type_str])
                    .inc();
            }
            Err((StatusCode::BAD_GATEWAY, format!("Request failed: {}", e)))
        }
    }
}

async fn process_upstream_response(
    response: reqwest::Response,
    peer_type_str: &str,
    metrics: &AppMetrics,
) -> Result<Response, (StatusCode, String)> {
    // Save the status code before consuming the response
    let status = response.status();
    let response_headers = response.headers().clone();

    let response_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!("Failed to read response body: {}", e);
            metrics
                .error_counts
                .with_label_values(&[peer_type_str, "response_body_read"])
                .inc();
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to read response body".to_string(),
            ));
        }
    };

    metrics
        .response_size_bytes
        .with_label_values(&[peer_type_str])
        .observe(response_bytes.len() as f64);

    // Create the response with the original status code
    let mut builder = Response::builder().status(status);

    // Copy headers from the original response
    for (name, value) in response_headers {
        if let Some(name) = name {
            builder = builder.header(name, value);
        }
    }

    // Build the response with proper error handling
    match builder.body(Body::from(response_bytes)) {
        Ok(resp) => Ok(resp),
        Err(e) => {
            warn!("Failed to build response: {}", e);
            metrics
                .error_counts
                .with_label_values(&[peer_type_str, "response_build_error"])
                .inc();
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to build response".to_string(),
            ))
        }
    }
}
