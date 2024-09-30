// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0
use fastcrypto::{ed25519::Ed25519PublicKey, secp256r1::Secp256r1PublicKey};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
use sui_tls::Allower;

/// WalrusPeers is a mapping of public key to WalrusPeer data
pub type WalrusPeers = Arc<RwLock<HashMap<Secp256r1PublicKey, WalrusPeer>>>;

#[derive(Hash, PartialEq, Eq, Debug, Clone)]
pub struct WalrusPeer {
    pub name: String,
    pub public_key: Secp256r1PublicKey,
}

/// AllowWalrus will allow walrus nodes
#[derive(Debug, Clone, Default)]
pub struct WalrusProvider {
    peers: WalrusPeers,
}

impl Allower for WalrusProvider {
    fn allowed(&self, key: &Ed25519PublicKey) -> bool {
        todo!()
    }
}

impl WalrusProvider {
    pub fn new(peers: Vec<WalrusPeer>) -> Self {
        // build our hashmap with the static pub keys. we only do this one time at binary startup.
        let statics: HashMap<Secp256r1PublicKey, WalrusPeer> = peers
            .into_iter()
            .map(|v| (v.public_key.clone(), v))
            .collect();
        Self {
            peers: Arc::new(RwLock::new(statics)),
        }
    }
}
