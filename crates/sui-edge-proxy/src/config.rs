use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct ProxyConfig {
    pub listen_address: String,
    pub metrics_address: Option<SocketAddr>,
    pub backends: BackendConfig,
    pub chain_identifier: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub struct BackendConfig {
    pub read_nodes: Vec<String>,
    pub execution_nodes: Vec<String>,
}

pub fn load<P: AsRef<std::path::Path>, T: DeserializeOwned + Serialize>(path: P) -> Result<T> {
    let path = path.as_ref();
    Ok(serde_yaml::from_reader(
        std::fs::File::open(path).context(format!("cannot open {:?}", path))?,
    )?)
}