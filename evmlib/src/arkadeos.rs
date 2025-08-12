#![allow(dead_code)]
#![allow(unused_variables)]
#![allow(unused_imports)]

use ark_bdk_wallet::Wallet;
use ark_rs::{
    client::{
        Blockchain, Client, Error as ArkClientError, ExplorerUtxo, ListVtxo, OffChainBalance,
        OfflineClient, SpendStatus,
        wallet::{Balance, BoardingWallet, OnchainWallet, Persistence},
    },
    core::{ArkAddress, BoardingOutput, UtxoCoinSelection, history::Transaction as ArkTransaction},
};
use bitcoin::{
    Address, Amount, FeeRate, Network, OutPoint, Psbt, Sequence, Transaction, Txid, XOnlyPublicKey,
    key::{Keypair, Secp256k1},
    secp256k1::{Message, SecretKey, schnorr::Signature},
};
use rand::prelude::*;
use std::{
    collections::HashMap,
    future::Future,
    str::FromStr,
    sync::{Arc, RwLock},
};

const NETWORK: Network = Network::Regtest;
const BITCOIND_RPC_URL: &str = "http://localhost:18443"; // bitcoind RPC server on nigiri
const BITCOIND_RPC_USER: &str = "admin1"; // bitcoind user on nigiri
const BITCOIND_RPC_PASSWORD: &str = "123"; // bitcoind password on nigiri

const ARK_SERVER_REGTEST: &str = "http://localhost:7070"; // local ARK operator on nigiri
const ARK_SERVER_MUTINYNET: &str = "https://mutinynet.arkade.sh";
const ARK_SERVER_SIGNET: &str = "https://signet.arkade.sh";
const BLOCKCHAIN_SERVER: &str = "http://localhost:3000"; // local Esplora on nigiri

#[derive(Debug, thiserror::Error)]
pub enum ArkadeosClientError {
    #[error("Ark client error")]
    ArkClientError(ArkClientError),
    #[error("bitcoin::secp256k1::Error")]
    Secp256k1Error(#[from] bitcoin::secp256k1::Error),
    #[error("extract txid error")]
    ExtractTxError(#[from] bitcoin::psbt::ExtractTxError),
    #[error("esplora client error")]
    EsploraClientError(#[from] esplora_client::Error),
}

pub struct BitcoinRpc {
    url: String,
    username: String,
    password: String,
    reqwest_client: reqwest::Client,
}

impl BitcoinRpc {
    pub fn new(url: String, username: String, password: String) -> Self {
        Self {
            url,
            username,
            password,
            reqwest_client: reqwest::Client::new(),
        }
    }

    pub async fn submit_package(&self, txs: Vec<String>) -> Result<(), ArkClientError> {
        let rpc_request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "submitpackage",
            "params": [txs]
        });

        let response = self
            .reqwest_client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .basic_auth(&self.username, Some(&self.password))
            .json(&rpc_request)
            .send()
            .await
            .map_err(ArkClientError::wallet)?;

        let status = response.status();
        let response_text = response.text().await.map_err(ArkClientError::wallet)?;

        if !status.is_success() {
            return Err(ArkClientError::wallet(format!(
                "Bitcoin RPC request failed with status {status}: {response_text}",
            )));
        }

        if response_text.contains("failed") {
            return Err(ArkClientError::wallet(format!(
                "Bitcoin RPC submitpackage failed: {response_text}",
            )));
        }

        // Parse JSON-RPC response to check for RPC-level errors
        let rpc_response: serde_json::Value = serde_json::from_str(&response_text)
            .map_err(|e| ArkClientError::wallet(format!("Failed to parse RPC response: {e}")))?;

        if let Some(error) = rpc_response.get("error") {
            return Err(ArkClientError::wallet(format!(
                "Bitcoin RPC submitpackage error: {error}",
            )));
        }

        println!(
            "Successfully submitted package of {} transactions",
            txs.len()
        );
        Ok(())
    }
}

struct MyBlockchain {
    pub server_url: String,
    esplora_client: esplora_client::AsyncClient,
    bitcoin_rpc: BitcoinRpc,
}

impl MyBlockchain {
    pub fn new(server_url: &str) -> Self {
        let builder = esplora_client::Builder::new(server_url);
        let esplora_client = builder
            .build_async()
            .expect("failed to build blockchain client");

        let bitcoin_rpc = BitcoinRpc::new(
            BITCOIND_RPC_URL.to_string(),
            BITCOIND_RPC_USER.to_string(),
            BITCOIND_RPC_PASSWORD.to_string(),
        );

        Self {
            server_url: server_url.to_string(),
            esplora_client,
            bitcoin_rpc,
        }
    }
}

impl Blockchain for MyBlockchain {
    fn find_outpoints(
        &self,
        address: &Address,
    ) -> impl Future<Output = Result<Vec<ExplorerUtxo>, ArkClientError>> + Send {
        async {
            let script_pubkey = address.script_pubkey();
            let txs = self
                .esplora_client
                .scripthash_txs(&script_pubkey, None)
                .await
                .expect("cannot get script hash");

            let outputs = txs
                .into_iter()
                .flat_map(|tx| {
                    let txid = tx.txid;
                    tx.vout
                        .iter()
                        .enumerate()
                        .filter(|(_, v)| v.scriptpubkey == script_pubkey)
                        .map(|(i, v)| ExplorerUtxo {
                            outpoint: OutPoint {
                                txid,
                                vout: i as u32,
                            },
                            amount: Amount::from_sat(v.value),
                            confirmation_blocktime: tx.status.block_time,
                            // Assume the output is unspent until we dig deeper, further down.
                            is_spent: false,
                        })
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>();

            let mut utxos = Vec::new();
            for output in outputs.iter() {
                let outpoint = output.outpoint;
                let status = self
                    .esplora_client
                    .get_output_status(&outpoint.txid, outpoint.vout as u64)
                    .await
                    .expect("cannot get output status");

                match status {
                    Some(esplora_client::OutputStatus { spent: false, .. }) | None => {
                        utxos.push(*output);
                    }
                    Some(esplora_client::OutputStatus { spent: true, .. }) => {
                        utxos.push(ExplorerUtxo {
                            is_spent: true,
                            ..*output
                        });
                    }
                }
            }

            Ok(utxos)
        }
    }

    fn find_tx(
        &self,
        txid: &Txid,
    ) -> impl Future<Output = Result<Option<Transaction>, ArkClientError>> + Send {
        async {
            let tx = self
                .esplora_client
                .get_tx(txid)
                .await
                .expect("failed to find tx");
            Ok(tx)
        }
    }

    fn get_output_status(
        &self,
        txid: &Txid,
        vout: u32,
    ) -> impl Future<Output = Result<SpendStatus, ArkClientError>> + Send {
        async move {
            let status = self
                .esplora_client
                .get_output_status(txid, vout as u64)
                .await
                .expect("cannot get script hash");

            Ok(SpendStatus {
                spend_txid: status.and_then(|s| s.txid),
            })
        }
    }

    fn broadcast(
        &self,
        tx: &Transaction,
    ) -> impl Future<Output = Result<(), ArkClientError>> + Send {
        async {
            self.esplora_client
                .broadcast(tx)
                .await
                .expect("failed to broadcast tx");
            Ok(())
        }
    }

    fn get_fee_rate(&self) -> impl Future<Output = Result<f64, ArkClientError>> + Send {
        async { Ok(1.0) }
    }

    fn broadcast_package(
        &self,
        txs: &[&Transaction],
    ) -> impl Future<Output = Result<(), ArkClientError>> + Send {
        async {
            let txs_hex = txs
                .iter()
                .map(bitcoin::consensus::encode::serialize_hex)
                .collect::<Vec<_>>();

            self.bitcoin_rpc.submit_package(txs_hex).await
        }
    }
}

#[derive(Default)]
pub struct InMemoryDb {
    boarding_outputs: RwLock<HashMap<BoardingOutput, SecretKey>>,
}

impl Persistence for InMemoryDb {
    fn save_boarding_output(
        &self,
        sk: SecretKey,
        boarding_output: BoardingOutput,
    ) -> Result<(), ArkClientError> {
        self.boarding_outputs
            .write()
            .unwrap()
            .insert(boarding_output, sk);

        Ok(())
    }

    fn load_boarding_outputs(&self) -> Result<Vec<BoardingOutput>, ArkClientError> {
        Ok(self
            .boarding_outputs
            .read()
            .unwrap()
            .keys()
            .cloned()
            .collect())
    }

    fn sk_for_pk(&self, pk: &XOnlyPublicKey) -> Result<SecretKey, ArkClientError> {
        let maybe_sk = self
            .boarding_outputs
            .read()
            .unwrap()
            .iter()
            .find_map(|(b, sk)| if b.owner_pk() == *pk { Some(*sk) } else { None });
        let secret_key = maybe_sk.unwrap();
        Ok(secret_key)
    }
}

pub struct ArkadeosClient {
    client: Client<MyBlockchain, Wallet<InMemoryDb>>,
}

impl ArkadeosClient {
    // Initialize the client
    pub async fn new(sk: Option<&str>) -> Result<Self, ArkadeosClientError> {
        // Create a keypair for signing transactions
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_str(
            sk.unwrap_or("e365d3ba2949840fcdc5099ea78aee408497f65005aa51e53ac7d1d837d5cb8a"),
        )?;
        //let (secret_key, public_key) = secp.generate_keypair(&mut rand::thread_rng());
        let keypair = Keypair::from_secret_key(&secp, &secret_key);

        // Initialize blockchain and wallet implementations
        let blockchain = Arc::new(MyBlockchain::new(BLOCKCHAIN_SERVER));
        let db = InMemoryDb::default();
        let wallet = Arc::new(
            Wallet::new(keypair, secp, NETWORK, BLOCKCHAIN_SERVER, db)
                .expect("failed to create wallet"),
        );

        // Create the offline client
        let offline_client = OfflineClient::new(
            "my-ark-client".to_string(),
            keypair,
            blockchain,
            wallet,
            ARK_SERVER_REGTEST.to_string(),
        );

        // Connect to the Ark server and get server info
        let client = offline_client
            .connect()
            .await
            .map_err(|err| ArkadeosClientError::ArkClientError(err))?;

        Ok(Self { client })
    }

    pub async fn get_offchain_address(&self) -> Result<ArkAddress, ArkadeosClientError> {
        let (ark_address, vtxo) = self
            .client
            .get_offchain_address()
            .map_err(|err| ArkadeosClientError::ArkClientError(err))?;

        Ok(ark_address)
    }

    pub async fn settle(&self) -> Result<(), ArkadeosClientError> {
        // TODO: check if there is balance for boarding
        self.client
            .settle(&mut rand::thread_rng(), true)
            .await
            .map_err(|err| ArkadeosClientError::ArkClientError(err))?;

        Ok(())
    }

    pub async fn list_vtxos(
        &self,
        include_recoverables: bool,
    ) -> Result<ListVtxo, ArkadeosClientError> {
        let vtxos = self
            .client
            .list_vtxos(include_recoverables)
            .await
            .map_err(|err| ArkadeosClientError::ArkClientError(err))?;
        //println!("Vtxos: {vtxos:#?}");

        let spendable_vtxos = self
            .client
            .spendable_vtxos(include_recoverables)
            .await
            .map_err(|err| ArkadeosClientError::ArkClientError(err))?;
        println!("Spendable Vtxos: {spendable_vtxos:#?}");

        Ok(vtxos)
    }

    pub async fn tx_history(&self) -> Result<Vec<ArkTransaction>, ArkadeosClientError> {
        let tx_history = self
            .client
            .transaction_history()
            .await
            .map_err(|err| ArkadeosClientError::ArkClientError(err))?;
        Ok(tx_history)
    }

    pub async fn get_offchain_balance(&self) -> Result<OffChainBalance, ArkadeosClientError> {
        let current_balance = self
            .client
            .offchain_balance()
            .await
            .map_err(|err| ArkadeosClientError::ArkClientError(err))?;

        Ok(current_balance)
    }

    pub async fn send_to(
        &self,
        amount: Amount,
        address: ArkAddress,
    ) -> Result<Txid, ArkadeosClientError> {
        let txid = self
            .client
            .send_vtxo(address, amount)
            .await
            .map_err(|err| ArkadeosClientError::ArkClientError(err))?;
        println!("Sent {amount} sats to {address} in transaction {txid}",);
        Ok(txid)
    }

    pub async fn wait_until_balance(&self, confirmed_target: Amount, pending_target: Amount) {
        tokio::time::timeout(tokio::time::Duration::from_secs(30), async {
            loop {
                let offchain_balance = self.client.offchain_balance().await.unwrap();

                println!(
                    "Waiting for balance to match targets: {offchain_balance:#?}\n target: {confirmed_target}, pending: {pending_target}"
                );

                if offchain_balance.confirmed() == confirmed_target
                    && offchain_balance.pending() == pending_target
                {
                    return;
                }

                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
            }
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEST_ADDR: &str = "609956fcc2d8bfd3efb1af44e1249fc9a0cc02f527454b77600afe156af15485";

    #[tokio::test]
    async fn test_arkadeos_client_get_offchain_addr() {
        let arkadeos_client = ArkadeosClient::new(None)
            .await
            .expect("cannot create ArkadeosClient");
        let arkadeos_client_dest = ArkadeosClient::new(Some(DEST_ADDR))
            .await
            .expect("cannot create ArkadeosClient");

        let balance = arkadeos_client
            .get_offchain_balance()
            .await
            .expect("failed to get balance");
        println!("Current offchain balance: {balance:#?}");

        let dest_balance = arkadeos_client_dest
            .get_offchain_balance()
            .await
            .expect("failed to get balance");
        println!("Current offchain balance dest: {dest_balance:#?}");

        let addr = arkadeos_client
            .get_offchain_address()
            .await
            .expect("failed to get offchain addr");
        println!(">>> ADDRESS: {addr}");
        assert!(!addr.encode().is_empty());
    }

    #[tokio::test]
    async fn test_arkadeos_client_settle_offchain_balance() {
        let arkadeos_client = ArkadeosClient::new(None)
            .await
            .expect("cannot create ArkadeosClient");

        let balance = arkadeos_client
            .get_offchain_balance()
            .await
            .expect("failed to get balance");
        println!("Current offchain balance: {balance:#?}");

        let tx_history = arkadeos_client
            .tx_history()
            .await
            .expect("failed to get tx history");
        println!("Transaction history: {tx_history:#?}");

        arkadeos_client
            .settle()
            .await
            .expect("failed to settle offchain balance");

        let new_balance = arkadeos_client
            .get_offchain_balance()
            .await
            .expect("failed to get balance");
        println!("Offchain balance after setting: {new_balance:#?}");
    }

    #[tokio::test]
    async fn test_arkadeos_client_send_to_other_client() {
        let amount = Amount::from_sat(400);

        let arkadeos_client = ArkadeosClient::new(None)
            .await
            .expect("cannot create ArkadeosClient");
        let arkadeos_client_dest = ArkadeosClient::new(Some(DEST_ADDR))
            .await
            .expect("cannot create ArkadeosClient");

        let src_addr = arkadeos_client
            .get_offchain_address()
            .await
            .expect("failed to get offchain addr");
        println!(">>> SRC ADDRESS: {src_addr}");

        let source_balance = arkadeos_client
            .get_offchain_balance()
            .await
            .expect("failed to get balance");
        println!("Current offchain balance source: {source_balance:#?}");
        assert!(
            amount <= source_balance.confirmed(),
            "not enough confirmed balance in source"
        );

        let dest_addr = arkadeos_client_dest
            .get_offchain_address()
            .await
            .expect("failed to get offchain addr");
        println!(">>> DEST ADDRESS: {dest_addr}");

        let dest_balance = arkadeos_client_dest
            .get_offchain_balance()
            .await
            .expect("failed to get balance");
        println!("Current offchain balance dest: {dest_balance:#?}");

        arkadeos_client
            .send_to(amount, dest_addr)
            .await
            .expect("failed to pay");

        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
        arkadeos_client_dest
            .wait_until_balance(dest_balance.confirmed(), dest_balance.pending() + amount)
            .await;
        arkadeos_client_dest
            .settle()
            .await
            .expect("failed to settle");

        /*let vtxos = arkadeos_client_dest
            .list_vtxos(true)
            .await
            .expect("failed to list vtxos");
        */

        let new_source_balance = arkadeos_client
            .get_offchain_balance()
            .await
            .expect("failed to get balance");
        println!("Current offchain balance source: {new_source_balance:#?}");
        let new_dest_balance = arkadeos_client_dest
            .get_offchain_balance()
            .await
            .expect("failed to get balance");
        println!("Current offchain balance dest: {new_dest_balance:#?}");

        assert_eq!(
            source_balance.confirmed() - amount,
            new_source_balance.confirmed()
        );
        assert_eq!(
            dest_balance.confirmed() + amount,
            new_dest_balance.confirmed()
        );
    }

    #[tokio::test]
    async fn test_arkadeos_client_send_to_address() {
        let arkadeos_client = ArkadeosClient::new(None)
            .await
            .expect("cannot create ArkadeosClient");

        let src_addr = arkadeos_client
            .get_offchain_address()
            .await
            .expect("failed to get offchain addr");
        println!(">>> SRC ADDRESS: {src_addr}");

        let source_balance = arkadeos_client
            .get_offchain_balance()
            .await
            .expect("failed to get balance");
        println!("Current offchain balance source: {source_balance:#?}");

        let dest_addr = ArkAddress::decode("tark1qq5thldh5pk3r3xlfhldsk0yjfpausje6d9jl0adpcz0jyw9gskeetnnksvyqrwptcrsjye5up6xjegh2klalh7ehma48p28fvvyd5gzftntx3").expect("invalid addr");

        let amount = Amount::from_sat(400);
        arkadeos_client
            .send_to(amount, dest_addr)
            .await
            .expect("failed to pay");

        let source_balance = arkadeos_client
            .get_offchain_balance()
            .await
            .expect("failed to get balance");
        println!("Current offchain balance source: {source_balance:#?}");
    }
}
