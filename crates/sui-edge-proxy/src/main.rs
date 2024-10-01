use bytes::Bytes;
use clap::Parser;
use pingora_load_balancing::LoadBalancer;
use serde_json::Value;
use sui_edge_proxy::config::ProxyConfig;

use async_trait::async_trait;
use pingora::{
    prelude::http_proxy_service,
    prelude::RoundRobin,
    prelude::{HttpPeer, ProxyHttp, Result, Session},
    server::{configuration::Opt, Server},
    services::{listening::Service as ListeningService, Service},
};

use std::sync::Arc;
use tracing::{debug, info};

#[derive(Parser, Debug)]
#[clap(rename_all = "kebab-case")]
struct Args {
    #[clap(
        long,
        short,
        default_value = "./sui-edge-proxy.yaml",
        help = "Specify the config file path to use"
    )]
    config: String,
}

fn main() -> Result<()> {
    // Initialize logging
    let (_guard, _handle) = telemetry_subscribers::TelemetryConfig::new().init();

    // Parse command-line arguments
    let args = Args::parse();

    let config: ProxyConfig = sui_edge_proxy::config::load(&args.config).unwrap();
    info!("{}", config.listen_address);

    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    let upstreams = LoadBalancer::try_from_iter(config.backends.read_nodes).unwrap();

    let mut lb = http_proxy_service(&my_server.configuration, LB(Arc::new(upstreams)));
    lb.add_tcp(&config.listen_address.as_str());
    my_server.add_service(lb);

    my_server.run_forever();
}

pub struct LB(Arc<LoadBalancer<RoundRobin>>);

#[async_trait]
impl ProxyHttp for LB {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        // Read the request body
        let body = session.read_request_body().await?;

        // Parse the JSON body
        if let Some(body_bytes) = body {
            if let Ok(json_body) = parse_json_body(&body_bytes) {
                if let Some("sui_executeTransactionBlock") =
                    json_body.get("method").and_then(|m| m.as_str())
                {
                    // Handle sui_executeTransactionBlock method
                    info!("Proxying request for sui_executeTransactionBlock");

                    let chain_identifier = Some("testnet");
                    let custom_upstream = match chain_identifier {
                        Some("mainnet") => "euw2-mainnet-benchmark-service.benchmark-rpc-mainnet.svc.clusterset.local",
                        Some("testnet") => "euw2-testnet-benchmark-service.benchmark-rpc-testnet.svc.clusterset.local",
                        _ => "euw2-testnet-benchmark-service.benchmark-rpc-testnet.svc.clusterset.local",
                    };

                    info!("Selected custom upstream: {}", custom_upstream);

                    // Create a new HttpPeer with the custom upstream
                    let peer = Box::new(HttpPeer::new(
                        (custom_upstream, 9000),
                        true,
                        custom_upstream.to_string(),
                    ));

                    return Ok(peer);
                }
            }
        }

        // Default behavior for other requests
        let upstream = self.0.select(b"", 256).unwrap();
        debug!("Default upstream peer is: {upstream:?}");

        let peer = Box::new(HttpPeer::new(upstream.clone(), true, upstream.to_string()));
        Ok(peer)
    }
}
fn parse_json_body(body: &Bytes) -> Result<Value, serde_json::Error> {
    serde_json::from_slice(body)
}
