// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use crate::peers::{JSON_RPC_DURATION, JSON_RPC_STATE};
use anyhow::{bail, Context, Result};
use fastcrypto::ed25519::Ed25519PublicKey;
use fastcrypto::encoding::Base64;
use fastcrypto::encoding::Encoding;
use fastcrypto::traits::ToFromBytes;
use futures::stream::{self, StreamExt};
use reqwest::Url;
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use sui_tls::Allower;
use sui_types::bridge::BridgeSummary;
use tracing::{debug, error, info};

/// BridgeValidators a mapping of public key to BridgeValidator data
pub type BridgeValidators = Arc<RwLock<HashMap<Ed25519PublicKey, BridgeValidator>>>;

#[derive(Hash, PartialEq, Eq, Debug, Clone)]
pub struct BridgeValidator {
    pub name: String,
    pub public_key: Ed25519PublicKey,
}

/// TODO
#[derive(Debug, Clone)]
pub struct BridgeValidatorProvider {
    nodes: BridgeValidators,
    rpc_url: String,
    rpc_poll_interval: Duration,
}

impl Allower for BridgeValidatorProvider {
    fn allowed(&self, key: &Ed25519PublicKey) -> bool {
        self.nodes.read().unwrap().contains_key(key)
    }
}

impl BridgeValidatorProvider {
    pub fn new(rpc_url: String, rpc_poll_interval: Duration) -> Self {
        let nodes = Arc::new(RwLock::new(HashMap::new()));
        Self {
            nodes,
            rpc_url,
            rpc_poll_interval,
        }
    }

    pub fn get(&self, key: &Ed25519PublicKey) -> Option<BridgeValidator> {
        debug!(?key, "fetching bridge validator for public key");
        if let Some(v) = self.nodes.read().unwrap().get(key) {
            return Some(BridgeValidator {
                name: v.name.to_owned(),
                public_key: v.public_key.to_owned(),
            });
        }
        None
    }

    /// get_validators will retrieve known bridge validators
    async fn get_validators(url: String) -> Result<BridgeSummary> {
        let rpc_method = "suix_getLatestBridge";
        let _timer = JSON_RPC_DURATION
            .with_label_values(&[rpc_method])
            .start_timer();
        let client = reqwest::Client::builder().build().unwrap();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "method":rpc_method,
            "id":1,
        });
        let response = client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(request.to_string())
            .send()
            .await
            .with_context(|| {
                JSON_RPC_STATE
                    .with_label_values(&[rpc_method, "failed_get"])
                    .inc();
                "unable to perform json rpc"
            })?;

        let raw = response.bytes().await.with_context(|| {
            JSON_RPC_STATE
                .with_label_values(&[rpc_method, "failed_body_extract"])
                .inc();
            "unable to extract body bytes from json rpc"
        })?;

        #[derive(Debug, Deserialize)]
        struct ResponseBody {
            result: BridgeSummary,
        }
        let summary: BridgeSummary = match serde_json::from_slice::<ResponseBody>(&raw) {
            Ok(b) => b.result,
            Err(error) => {
                JSON_RPC_STATE
                    .with_label_values(&[rpc_method, "failed_json_decode"])
                    .inc();
                bail!(
                    "unable to decode json: {error} response from json rpc: {:?}",
                    raw
                )
            }
        };
        JSON_RPC_STATE
            .with_label_values(&[rpc_method, "success"])
            .inc();
        Ok(summary)
    }

    /// poll_peer_list will act as a refresh interval for our cache
    pub fn poll_peer_list(&self) {
        info!(
            self.rpc_url,
            ?self.rpc_poll_interval, "Started polling for active bridge validators"
        );

        let rpc_poll_interval = self.rpc_poll_interval;
        let rpc_url = self.rpc_url.to_owned();
        let nodes = self.nodes.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(rpc_poll_interval);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;

                match Self::get_validators(rpc_url.to_owned()).await {
                    Ok(summary) => {
                        let extracted = extract_bridge(summary).await;
                        let mut allow = nodes.write().unwrap();
                        allow.clear();
                        allow.extend(extracted);
                        info!(
                            "{} sui bridge peers managed to make it on the allow list",
                            allow.len()
                        );

                        JSON_RPC_STATE
                            .with_label_values(&["update_bridge_peer_count", "success"])
                            .inc_by(allow.len() as f64);
                    }
                    Err(error) => {
                        JSON_RPC_STATE
                            .with_label_values(&["update_bridge_peer_count", "failed"])
                            .inc();
                        error!("unable to refresh sui bridge peer list: {error}")
                    }
                }
            }
        });
    }
}

async fn extract_bridge(summary: BridgeSummary) -> Vec<(Ed25519PublicKey, BridgeValidator)> {
    let client = reqwest::Client::builder().build().unwrap();
    let results: Vec<_> = stream::iter(summary.committee.members)
        .filter_map(|(_, cm)| {
            let client = client.clone();
            async move {
                debug!(
                    address =% cm.sui_address,
                    "Extracting metrics public key for bridge node",
                );

                // Convert the Vec<u8> to a String and handle errors properly
                let url_str = match String::from_utf8(cm.http_rest_url) {
                    Ok(url) => url,
                    Err(_) => {
                        error!(
                            address =% cm.sui_address,
                            "Invalid UTF-8 sequence in http_rest_url for bridge node ",
                        );
                        return None;
                    }
                };
                // Parse the URL
                let mut bridge_url = match Url::parse(&url_str) {
                    Ok(url) => url,
                    Err(_) => {
                        error!(url_str, "Unable to parse http_rest_url");
                        return None;
                    }
                };
                bridge_url.set_path("/metrics_pub_key");

                // use the host portion of the http_rest_url as the "name"
                let bridge_host = match bridge_url.host_str() {
                    Some(host) => host,
                    None => {
                        error!(url_str, address =% cm.sui_address, "Hostname is missing from http_rest_url");
                        return None;
                    }
                };
                let bridge_name = String::from(bridge_host);
                let bridge_request_url = bridge_url.as_str();

                let response = client.get(bridge_request_url).send().await.ok()?;
                let raw = response.bytes().await.ok()?;
                let metrics_pub_key: String = match serde_json::from_slice(&raw) {
                    Ok(key) => key,
                    Err(error) => {
                        error!(?error, "Failed to deserialize response");
                        return None;
                    }
                };
                let metrics_bytes = match Base64::decode(&metrics_pub_key) {
                    Ok(pubkey_bytes) => pubkey_bytes,
                    Err(error) => {
                        error!(
                            ?error,
                            bridge_name, "unable to decode public key for bridge node",
                        );
                        return None;
                    }
                };
                match Ed25519PublicKey::from_bytes(&metrics_bytes) {
                    Ok(metrics_key) => {
                        debug!(bridge_request_url, ?metrics_key, "adding metrics key");
                        Some((
                            metrics_key.clone(),
                            BridgeValidator {
                                public_key: metrics_key.clone(),
                                name: bridge_name,
                            },
                        ))
                    }
                    Err(error) => {
                        error!(
                            ?error,
                            bridge_request_url, "unable to decode public key for bridge node",
                        );
                        None
                    }
                }
            }
        })
        .collect()
        .await;

    results
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::{generate_self_cert, CertKeyPair};
    use serde::Serialize;
    use sui_types::sui_system_state::sui_system_state_summary::{
        SuiSystemStateSummary, SuiValidatorSummary,
    };

    /// creates a test that binds our proxy use case to the structure in sui_getLatestSuiSystemState
    /// most of the fields are garbage, but we will send the results of the serde process to a private decode
    /// function that should always work if the structure is valid for our use
    #[test]
    fn depend_on_sui_sui_system_state_summary() {
        todo!();
        // let CertKeyPair(_, client_pub_key) = generate_self_cert("sui".into());
        // let p2p_address: Multiaddr = "/ip4/127.0.0.1/tcp/10000"
        //     .parse()
        //     .expect("expected a multiaddr value");
        // // all fields here just satisfy the field types, with exception to active_validators, we use
        // // some of those.
        // let depends_on = SuiSystemStateSummary {
        //     active_validators: vec![SuiValidatorSummary {
        //         network_pubkey_bytes: Vec::from(client_pub_key.as_bytes()),
        //         p2p_address: format!("{p2p_address}"),
        //         primary_address: "empty".into(),
        //         worker_address: "empty".into(),
        //         ..Default::default()
        //     }],
        //     ..Default::default()
        // };

        // #[derive(Debug, Serialize, Deserialize)]
        // struct ResponseBody {
        //     result: SuiSystemStateSummary,
        // }

        // let r = serde_json::to_string(&ResponseBody { result: depends_on })
        //     .expect("expected to serialize ResponseBody{SuiSystemStateSummary}");

        // let deserialized = serde_json::from_str::<ResponseBody>(&r)
        //     .expect("expected to deserialize ResponseBody{SuiSystemStateSummary}");

        // let peers = extract(deserialized.result);
        // assert_eq!(peers.count(), 1, "peers should have been a length of 1");
    }
}
