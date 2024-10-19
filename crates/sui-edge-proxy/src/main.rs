use bytes::Bytes;
use clap::Parser;
use http::{Response, StatusCode};
use pingora::apps::http_app::ServeHttp;
use sui_edge_proxy::config::ProxyConfig;
use sui_edge_proxy::{certificate::TLSCertCallback, config::PeerConfig};

use async_trait::async_trait;
use pingora::{
    listeners::TlsSettings,
    prelude::{http_proxy_service, HttpPeer, ProxyHttp, Result, Session},
    server::Server,
    services::listening::Service,
};
// TODO: look into using pingora_load_balancing
// use pingora_load_balancing::{health_check, selection::RoundRobin, LoadBalancer};

// use pingora::protocols::http::error_resp;

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

// testing tls termination
// RUST_LOG=debug cargo run --bin sui-edge-proxy -- --config=crates/sui-edge-proxy/proxy.yaml
// mkdir -p keys
// openssl req -x509 -sha256 -days 356 -nodes -newkey rsa:2048 -subj "/CN=fullnode.mainnet.sui.io/C=UK/L=London" -keyout keys/key.pem -out keys/cert.crt

fn main() -> Result<()> {
    let (_guard, _handle) = telemetry_subscribers::TelemetryConfig::new().init();
    let args = Args::parse();

    let config: ProxyConfig = sui_edge_proxy::config::load(&args.config).unwrap();
    info!("listening on {}", config.listen_address);

    let mut my_server = Server::new(None).unwrap();
    my_server.bootstrap();

    // Add health check service
    let health_check = health_check_service(&format!("0.0.0.0:9000"));
    my_server.add_service(health_check);

    let mut lb = http_proxy_service(
        &my_server.configuration,
        LB(config.read_peer, config.execution_peer),
    );

    if let Some(tls_config) = config.tls {
        info!("TLS config found");
        let cert_callback =
            TLSCertCallback::new(tls_config.cert_path, tls_config.key_path, tls_config.sni);
        let cert_callback = Box::new(cert_callback);
        // TODO error handling
        let tls_config = TlsSettings::with_callbacks(cert_callback).unwrap();
        lb.add_tls_with_settings(&config.listen_address.as_str(), None, tls_config);
    } else {
        info!("No TLS config found");
        lb.add_tcp(&config.listen_address.as_str());
    }

    my_server.add_service(lb);

    my_server.run_forever();
}

pub struct LB(PeerConfig, PeerConfig);

#[async_trait]
impl ProxyHttp for LB {
    type CTX = ();
    fn new_ctx(&self) -> Self::CTX {}

    async fn upstream_peer(
        &self,
        session: &mut Session,
        _ctx: &mut Self::CTX,
    ) -> Result<Box<HttpPeer>> {
        info!("upstream_peer");

        // Check if the "transaction_type" header is set to "execute"
        if let Some(transaction_type) = session.req_header().headers.get("Sui-Transaction-Type") {
            match transaction_type.to_str() {
                Ok("execute") => {
                    info!("Proxying request for execute transaction");

                    // Create a new HttpPeer with the custom upstream
                    let peer = Box::new(HttpPeer::new(
                        self.1.address.clone(),
                        self.1.use_tls,
                        self.1.sni.clone(),
                    ));

                    info!("returning read peer");
                    debug!("{:?}", peer);
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
        // Create a new HttpPeer with the read peer
        let peer = Box::new(HttpPeer::new(
            self.0.address.clone(),
            self.0.use_tls,
            self.0.sni.clone(),
        ));
        info!("returning read peer");
        debug!("{:?}", peer);
        Ok(peer)
    }
}

// Add this new function for the health check service
fn health_check_service(listen_addr: &str) -> Service<HealthCheckApp> {
    let mut service = Service::new("Health Check Service".to_string(), HealthCheckApp {});
    service.add_tcp(listen_addr);
    service
}

pub struct HealthCheckApp;

#[async_trait]
impl ServeHttp for HealthCheckApp {
    async fn response(
        &self,
        _http_stream: &mut pingora::protocols::http::ServerSession,
    ) -> Response<Vec<u8>> {
        let body = Bytes::from("up");

        Response::builder()
            .status(StatusCode::OK)
            .header(http::header::CONTENT_TYPE, "text/html")
            .header(http::header::CONTENT_LENGTH, body.len())
            .body(body.to_vec())
            .unwrap()
    }
}
