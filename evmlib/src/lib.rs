// Copyright 2024 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

// Allow expect usage in this crate as it's used for compile-time constants
#![allow(clippy::expect_used)]
// Allow enum variant names that end with Error as they come from external derives
#![allow(clippy::enum_variant_names)]
use crate::common::{Amount, QuoteHash};
use crate::contract::payment_vault::{
    error, handler::PaymentVaultHandler, interface::IPaymentVault,
};
use crate::quoting_metrics::QuotingMetrics;
use crate::utils::http_provider;

use crate::common::Address;
use crate::utils::get_evm_network;
use alloy::primitives::address;
use alloy::transports::http::reqwest;
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use std::str::FromStr;
use std::sync::LazyLock;

#[macro_use]
extern crate tracing;

pub mod common;
pub mod contract;
pub mod cryptography;
#[cfg(feature = "external-signer")]
pub mod external_signer;
pub mod quoting_metrics;
mod retry;
pub mod testnet;
pub mod transaction_config;
pub mod utils;
pub mod wallet;

/// Timeout for transactions
const TX_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(24); // Should differ per chain

static PUBLIC_ARBITRUM_ONE_HTTP_RPC_URL: LazyLock<reqwest::Url> = LazyLock::new(|| {
    "https://arb1.arbitrum.io/rpc"
        .parse()
        .expect("Invalid RPC URL")
});

static PUBLIC_ARBITRUM_SEPOLIA_HTTP_RPC_URL: LazyLock<reqwest::Url> = LazyLock::new(|| {
    "https://sepolia-rollup.arbitrum.io/rpc"
        .parse()
        .expect("Invalid RPC URL")
});

static PUBLIC_LND_HTTP_RPC_URL: LazyLock<reqwest::Url> = LazyLock::new(|| {
    "https:///localhost:13002"
        .parse()
        .expect("Invalid LND RPC URL")
});

const ARBITRUM_ONE_PAYMENT_TOKEN_ADDRESS: Address =
    address!("a78d8321B20c4Ef90eCd72f2588AA985A4BDb684");

const ARBITRUM_SEPOLIA_TEST_PAYMENT_TOKEN_ADDRESS: Address =
    address!("4bc1aCE0E66170375462cB4E6Af42Ad4D5EC689C");

const ARBITRUM_ONE_DATA_PAYMENTS_ADDRESS: Address =
    address!("B1b5219f8Aaa18037A2506626Dd0406a46f70BcC");

const ARBITRUM_SEPOLIA_TEST_DATA_PAYMENTS_ADDRESS: Address =
    address!("7f0842a78f7d4085d975ba91d630d680f91b1295");

#[serde_as]
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomNetwork {
    #[serde_as(as = "DisplayFromStr")]
    pub rpc_url_http: reqwest::Url,
    pub payment_token_address: Address,
    pub data_payments_address: Address,
}

impl CustomNetwork {
    pub fn new(rpc_url: &str, payment_token_addr: &str, data_payments_addr: &str) -> Self {
        Self {
            rpc_url_http: reqwest::Url::parse(rpc_url).expect("Invalid RPC URL"),
            payment_token_address: Address::from_str(payment_token_addr)
                .expect("Invalid payment token address"),
            data_payments_address: Address::from_str(data_payments_addr)
                .expect("Invalid chunk payments address"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum Network {
    #[default]
    ArbitrumOne,
    ArbitrumSepoliaTest,
    LightningNetwork,
    Custom(CustomNetwork),
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Network::ArbitrumOne => write!(f, "evm-arbitrum-one"),
            Network::ArbitrumSepoliaTest => write!(f, "evm-arbitrum-sepolia-test"),
            Network::LightningNetwork => write!(f, "lightning-network"),
            Network::Custom(_) => write!(f, "evm-custom"),
        }
    }
}

impl std::str::FromStr for Network {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "evm-arbitrum-one" => Ok(Network::ArbitrumOne),
            "evm-arbitrum-sepolia-test" => Ok(Network::ArbitrumSepoliaTest),
            "lightning-network" => Ok(Network::LightningNetwork),
            _ => Err(()),
        }
    }
}

impl Network {
    pub fn new(local: bool) -> Result<Self, utils::Error> {
        get_evm_network(local, None).inspect_err(|err| {
            warn!("Failed to select EVM network from ENV: {err}");
        })
    }

    pub fn new_custom(rpc_url: &str, payment_token_addr: &str, chunk_payments_addr: &str) -> Self {
        Self::Custom(CustomNetwork::new(
            rpc_url,
            payment_token_addr,
            chunk_payments_addr,
        ))
    }

    pub fn identifier(&self) -> &str {
        match self {
            Network::ArbitrumOne => "arbitrum-one",
            Network::ArbitrumSepoliaTest => "arbitrum-sepolia-test",
            Network::LightningNetwork => "lightning-network",
            Network::Custom(_) => "custom",
        }
    }

    pub fn rpc_url(&self) -> &reqwest::Url {
        match self {
            Network::ArbitrumOne => &PUBLIC_ARBITRUM_ONE_HTTP_RPC_URL,
            Network::ArbitrumSepoliaTest => &PUBLIC_ARBITRUM_SEPOLIA_HTTP_RPC_URL,
            Network::LightningNetwork => &PUBLIC_LND_HTTP_RPC_URL,
            Network::Custom(custom) => &custom.rpc_url_http,
        }
    }

    pub fn payment_token_address(&self) -> &Address {
        match self {
            Network::ArbitrumOne => &ARBITRUM_ONE_PAYMENT_TOKEN_ADDRESS,
            Network::ArbitrumSepoliaTest => &ARBITRUM_SEPOLIA_TEST_PAYMENT_TOKEN_ADDRESS,
            Network::Custom(custom) => &custom.payment_token_address,
            Network::LightningNetwork => todo!("PAYMENT ADDRESS MISSING"),
        }
    }

    pub fn data_payments_address(&self) -> &Address {
        match self {
            Network::ArbitrumOne => &ARBITRUM_ONE_DATA_PAYMENTS_ADDRESS,
            Network::ArbitrumSepoliaTest => &ARBITRUM_SEPOLIA_TEST_DATA_PAYMENTS_ADDRESS,
            Network::Custom(custom) => &custom.data_payments_address,
            Network::LightningNetwork => {
                todo!("DATA PAYMENT ADDRESS NOT SUPPORTED FOR LIGHTNING NETWORK")
            }
        }
    }

    /// Add LN invoice if netowkr is LightningNetwork
    pub async fn add_invoice(&self, amount: Amount) -> String {
        // TODO: send request to LND node to create an invoice
        format!("TODO-create-LN-invoice-price-{amount}")
    }

    /// Helper function to return a quote for the given quoting metrics.
    pub async fn get_market_price(
        &self,
        quoting_metrics: Vec<QuotingMetrics>,
    ) -> Result<Vec<Amount>, error::Error> {
        if matches!(self, Self::LightningNetwork) {
            Ok(quoting_metrics
                .iter()
                .map(|q| {
                    // FIXME: use a proper quoting formula
                    let amt = Amount::from(q.data_size / 100);
                    println!(">>>> QUOTE for size {}: {amt}", q.data_size);
                    amt
                })
                .collect())
        } else {
            let provider = http_provider(self.rpc_url().clone());
            let payment_vault = PaymentVaultHandler::new(*self.data_payments_address(), provider);
            payment_vault.get_quote(quoting_metrics).await
        }
    }

    /// Helper function to verify whether a data payment is valid.
    /// Returns the amount paid to the owned quote hashes.
    pub async fn verify_data_payment(
        &self,
        owned_quote_hashes: Vec<QuoteHash>,
        payment: Vec<(QuoteHash, QuotingMetrics, Address)>,
    ) -> Result<Amount, error::Error> {
        if matches!(self, Self::LightningNetwork) {
            // TODO: query the LN node to see if the invoice has been payed
            Err(error::Error::PaymentInvalid)
        } else {
            let provider = http_provider(self.rpc_url().clone());
            let payment_vault = PaymentVaultHandler::new(*self.data_payments_address(), provider);

            let mut amount = Amount::ZERO;

            let payment_verifications: Vec<_> = payment
                .into_iter()
                .map(IPaymentVault::PaymentVerification::from)
                .collect();

            let payment_verification_results =
                payment_vault.verify_payment(payment_verifications).await?;

            for payment_verification_result in payment_verification_results {
                // TODO we currently fail on a single invalid payment, maybe we should deal with this in a different way
                if !payment_verification_result.isValid {
                    return Err(error::Error::PaymentInvalid);
                }

                if owned_quote_hashes.contains(&payment_verification_result.quoteHash) {
                    amount += payment_verification_result.amountPaid;
                }
            }

            Ok(amount)
        }
    }
}
