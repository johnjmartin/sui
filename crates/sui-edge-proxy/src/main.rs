use bytes::{BufMut, Bytes, BytesMut};
use clap::Parser;
use serde_json::Value;
use sui_edge_proxy::config::ProxyConfig;
use sui_edge_proxy::{certificate::TLSCertCallback, config::PeerConfig};

use async_trait::async_trait;
use pingora::{
    http::ResponseHeader,
    listeners::TlsSettings,
    prelude::{http_proxy_service, HttpPeer, ProxyHttp, Result, Session},
    server::Server,
};
use pingora_load_balancing::{health_check, selection::RoundRobin, LoadBalancer};

// use pingora::protocols::http::error_resp;

use std::sync::Arc;
use tracing::{debug, info, warn};

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

// RUST_LOG=debug cargo run --bin sui-edge-proxy -- --config=crates/sui-edge-proxy/proxy.yaml
// mkdir -p keys
// openssl req -x509 -sha256 -days 356 -nodes -newkey rsa:2048 -subj "/CN=fullnode.mainnet.sui.io/C=UK/L=London" -keyout keys/key.pem -out keys/cert.crt

fn main() -> Result<()> {
    // Initialize logging
    let (_guard, _handle) = telemetry_subscribers::TelemetryConfig::new().init();

    // Parse command-line arguments
    let args = Args::parse();

    let config: ProxyConfig = sui_edge_proxy::config::load(&args.config).unwrap();
    info!("{}", config.listen_address);
    info!("{:?}", config.backends.read_nodes);

    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    // todo: error handling
    let upstreams = LoadBalancer::try_from_iter(config.backends.read_nodes).unwrap();

    let mut lb = http_proxy_service(
        &my_server.configuration,
        LB(
            Arc::new(upstreams),
            config.custom_peer.clone(),
            config.default_peer.clone(),
            config.local_override,
        ),
    );

    if let Some(tls_config) = config.tls {
        let cert_callback =
            TLSCertCallback::new(tls_config.cert_path, tls_config.key_path, tls_config.sni);
        let cert_callback = Box::new(cert_callback);
        // TODO error handling
        let tls_config = TlsSettings::with_callbacks(cert_callback).unwrap();
        lb.add_tls_with_settings(&config.listen_address.as_str(), None, tls_config);
    } else {
        lb.add_tcp(&config.listen_address.as_str());
    }

    my_server.add_service(lb);

    my_server.run_forever();
}

pub struct LB(Arc<LoadBalancer<RoundRobin>>, PeerConfig, PeerConfig, bool);

#[async_trait]
impl ProxyHttp for LB {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    // async fn request_filter(&self, session: &mut Session, _ctx: &mut Self::CTX) -> Result<bool> {
    //     info!("request_filter");
    //     // session.read_request().await?;
    //     info!("{:?}", session.req_header());

    //     let body = session.read_request_body().await?;
    //     debug!("{:?}", body);
    //     debug!("{:?}", session.body_bytes_read());
    //     debug!("{:?}", session.body_bytes_sent());
    //     session.finish_body().await?;
    //     debug!("{:?}", session.is_body_empty());
    //     session
    //         .write_response_body(Some(body.unwrap()), true)
    //         .await?;
    //     let mut resp = error_resp::gen_error_response(200);
    //     resp.insert_header("john", "istesting").unwrap();
    //     session
    //         // expected struct `Box<ResponseHeader>`
    //         .write_response_header(Box::new(ResponseHeader::from(resp)), true)
    //         .await?;

    //     Ok(false)
    // }

    // async fn request_body_filter(
    //     &self,
    //     _session: &mut Session,
    //     body: &mut Option<Bytes>,
    //     _end_of_stream: bool,
    //     ctx: &mut Self::CTX,
    // ) -> pingora::Result<()>
    // where
    //     Self::CTX: Send + Sync,
    // {
    //     info!("request_body_filter");
    //     debug!("{:?}", body);
    //     debug!("{:?}", ctx);
    //     Ok(())
    // }

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        info!("upstream_peer");

        // Check if the "transaction_type" header is set to "execute"
        if let Some(transaction_type) = session.req_header().headers.get("transaction_type") {
            match transaction_type.to_str() {
                Ok("execute") => {
                    info!("Proxying request for execute transaction");

                    let chain_identifier = Some("testnet");
                    let custom_upstream = match chain_identifier {
                        Some("mainnet") => {
                            "euw2-mainnet-benchmark-service.benchmark-rpc-mainnet.svc.clusterset.local"
                        }
                        Some("testnet") => {
                            "euw2-testnet-benchmark-service.benchmark-rpc-testnet.svc.clusterset.local"
                        }
                        _ => {
                            "euw2-testnet-benchmark-service.benchmark-rpc-testnet.svc.clusterset.local"
                        }
                    };

                    info!("Selected custom upstream: {}", custom_upstream);

                    // Create a new HttpPeer with the custom upstream
                    let peer = Box::new(HttpPeer::new(
                        (custom_upstream, 9000),
                        false,
                        custom_upstream.to_string(),
                    ));

                    return Ok(peer);
                }
                Ok(_) => {
                    // Transaction type is not "execute", continue with default behavior
                }
                Err(e) => {
                    // Log the error and continue with default behavior
                    warn!("Failed to read transaction_type header: {}", e);
                }
            }
        }
        // hack if I can't get the header set in the benchmark setup
        if self.3 {
            // Create a new HttpPeer with the custom upstream
            info!("Proxying request for execute transaction with local override");
            return Ok(Box::new(HttpPeer::new(
                self.1.address.clone(),
                self.1.use_tls,
                self.1.sni.clone(),
            )));
        }

        // Default behavior for other requests
        //  let peer = Box::new(HttpPeer::new(
        //         upstream,
        //         true,
        //         String::from("fullnode.mainnet.sui.io"),
        //     ));
        //     debug!("returning fullnode.mainnet.sui.io:443 peer");
        let upstream = self.0.select(b"", 256).unwrap();
        info!("Default upstream peer is: {upstream:?}");

        let peer = Box::new(HttpPeer::new(upstream, self.2.use_tls, self.2.sni.clone()));
        debug!("returning default peer");
        debug!("{:?}", peer);
        Ok(peer)
    }

    // async fn response_filter(
    //     &self,
    //     _session: &mut Session,
    //     upstream_response: &mut ResponseHeader,
    //     _ctx: &mut Self::CTX,
    // ) -> Result<()> {
    //     // replace existing header if any
    //     upstream_response
    //         .insert_header("Server", "MyGateway")
    //         .unwrap();
    //     // because we don't support h3
    //     upstream_response.remove_header("alt-svc");
    //     info!("{:?}", upstream_response);

    //     Ok(())
    // }
}

fn parse_json_body(body: &Bytes) -> Result<Value, serde_json::Error> {
    serde_json::from_slice(body)
}
