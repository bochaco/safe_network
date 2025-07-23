use crate::{Amount, utils::Error};

use serde::{Deserialize, Serialize};
use std::{fmt, fs};

const TAPROOT_ASSET_ID: &str = "d81d8d1a2c0e96726b430b818bf7dfbf543e07b812520aba53f63e3cfd4d041e";
const TAPROOT_ASSETS_DIR: &str = "~/.polar/networks/1/volumes/litd/bob";
const LND_DIR: &str = "~/.polar/networks/1/volumes/litd/bob/lnd";
const LND_HOST: &str = "localhost:13002";

#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub struct LnClient {
    asset_id: Vec<u8>,
    cert: String,
    tapd_macaroon: String,
    lnd_macaroon: String,
    lnd_host: String,
}

impl fmt::Debug for LnClient {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("LnClient")
            .field("lnd_host", &self.lnd_host)
            .finish()
    }
}

impl LnClient {
    pub fn new() -> Result<Self, Error> {
        let asset_id = hex::decode(TAPROOT_ASSET_ID)
            .map_err(|err| {
                Error::FailedToGetLnInvoice(format!("Failed to decode Taproot asset-id: {err:?}"))
            })?
            .into();

        let tapd_macaroon_path = format!("{TAPROOT_ASSETS_DIR}/tapd/data/regtest/admin.macaroon");
        let lnd_macaroon_path = format!("{LND_DIR}/data/chain/bitcoin/regtest/admin.macaroon");
        let tls_path = format!("{LND_DIR}/tls.cert");

        // Read the contents of the file into a vector of bytes
        let cert_bytes = fs::read(&tls_path).map_err(|err| {
            Error::FailedToGetLnInvoice(format!("Failed to read LND TLS Certificate file: {err:?}"))
        })?;
        let tapd_mac_bytes = fs::read(&tapd_macaroon_path).map_err(|err| {
            Error::FailedToGetLnInvoice(format!(
                "Failed to read LND Macaroon file {tapd_macaroon_path}: {err:?}"
            ))
        })?;
        let lnd_mac_bytes = fs::read(&lnd_macaroon_path).map_err(|err| {
            Error::FailedToGetLnInvoice(format!(
                "Failed to read LND Macaroon file {lnd_macaroon_path}: {err:?}"
            ))
        })?;

        // Convert the bytes to a hex string
        let cert = buffer_as_hex(cert_bytes);
        let tapd_macaroon = buffer_as_hex(tapd_mac_bytes);
        let lnd_macaroon = buffer_as_hex(lnd_mac_bytes);
        let lnd_host = LND_HOST.to_string();

        Ok(Self {
            asset_id,
            cert,
            tapd_macaroon,
            lnd_macaroon,
            lnd_host,
        })
    }

    // Helper to connect to LND and return the connected client
    async fn get_client(&self) -> Result<lnd_grpc_rust::LndClient, Error> {
        lnd_grpc_rust::connect(
            self.cert.clone(),
            self.tapd_macaroon.clone(),
            self.lnd_host.clone(),
        )
        .await
        .map_err(|err| Error::FailedToGetLnInvoice(format!("Failed to connect to LND: {err:?}")))
    }

    // Send request to LND node to create a new invoice
    pub async fn add_invoice(
        &self,
        memo: impl Into<String>,
        amount: Amount,
    ) -> Result<String, Error> {
        let amt = if amount.is_zero() {
            return Err(Error::FailedToGetLnInvoice("Amount is zero".to_string()));
        } else {
            let amt = amount.saturating_to::<i64>() as u64;
            amt * 1000 // FIXME: why it needs to be greater than 400
        };

        let invoice_request = Some(lnd_grpc_rust::lnrpc::Invoice {
            memo: memo.into(),
            ..Default::default()
        });
        let request = lnd_grpc_rust::tapchannelrpc::AddInvoiceRequest {
            asset_id: self.asset_id.clone(),
            asset_amount: amt,
            invoice_request,
            ..Default::default()
        };

        let info = self
            .get_client()
            .await?
            .tapchannel()
            .add_invoice(request)
            .await
            .map_err(|err| {
                Error::FailedToGetLnInvoice(format!("Failed to add invoice: {err:?}"))
            })?;

        if let Some(invoice_result) = info.into_inner().invoice_result {
            Ok(invoice_result.payment_request)
        } else {
            Err(Error::FailedToGetLnInvoice(
                "Empty invoice returned by LND".to_string(),
            ))
        }
    }
}

fn buffer_as_hex(bytes: Vec<u8>) -> String {
    let hex_str = bytes
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();

    return hex_str;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ln_client_add_invoice() {
        let ln_client = LnClient::new().expect("cannot create LnClient");

        let amt = Amount::from(3);
        let invoice = ln_client
            .add_invoice("invoice-1", amt)
            .await
            .expect("couldn't add invoice");
        println!(">>> INVOICE for {amt}: {invoice}");
        assert!(!invoice.is_empty());

        let amt = Amount::from(0);
        assert!(ln_client.add_invoice("invoice-1", amt).await.is_err());
    }
}
