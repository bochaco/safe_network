use crate::{Amount, utils::Error};

use futures_util::stream::StreamExt;
use lnd_grpc_rust::{
    lnrpc::{self, Payment, PaymentFailureReason, payment::PaymentStatus},
    tapchannelrpc,
    tapchannelrpc::send_payment_response::Result::{AcceptedSellOrder, PaymentResult},
};
use serde::{Deserialize, Serialize};
use std::{
    fmt, fs,
    path::{Path, PathBuf},
};

const TAPROOT_ASSET_ID: &str = "d81d8d1a2c0e96726b430b818bf7dfbf543e07b812520aba53f63e3cfd4d041e";
const TAPROOT_ASSETS_DIR_BOB: &str = "~/.polar/networks/1/volumes/litd/bob";
const TAPROOT_ASSETS_DIR_ALICE: &str = "~/.polar/networks/1/volumes/litd/alice";
const TAPROOT_ASSETS_DIR_CAROL: &str = "~/.polar/networks/1/volumes/litd/carol";
const LND_HOST_ALICE: &str = "localhost:13001";
const LND_HOST_BOB: &str = "localhost:13002";
const LND_HOST_CAROL: &str = "localhost:13003";

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
    pub fn new(base_path: Option<&Path>, host: Option<String>) -> Result<Self, Error> {
        let asset_id = hex::decode(TAPROOT_ASSET_ID)
            .map_err(|err| {
                Error::FailedToGetLnInvoice(format!("Failed to decode Taproot asset-id: {err:?}"))
            })?
            .into();

        let base_path = match base_path {
            Some(p) => p.to_path_buf(),
            None => PathBuf::from(TAPROOT_ASSETS_DIR_BOB),
        };

        let tapd_macaroon_path = base_path.join("tapd/data/regtest/admin.macaroon");

        let lnd_dir = base_path.join("lnd");
        let lnd_macaroon_path = lnd_dir.join("data/chain/bitcoin/regtest/admin.macaroon");
        let tls_path = lnd_dir.join("tls.cert");

        // Read the contents of the file into a vector of bytes
        let cert_bytes = fs::read(&tls_path).map_err(|err| {
            Error::FailedToGetLnInvoice(format!(
                "Failed to read LND TLS Certificate file {tls_path:?}: {err:?}"
            ))
        })?;
        let tapd_mac_bytes = fs::read(&tapd_macaroon_path).map_err(|err| {
            Error::FailedToGetLnInvoice(format!(
                "Failed to read LND Macaroon file {tapd_macaroon_path:?}: {err:?}"
            ))
        })?;
        let lnd_mac_bytes = fs::read(&lnd_macaroon_path).map_err(|err| {
            Error::FailedToGetLnInvoice(format!(
                "Failed to read LND Macaroon file {lnd_macaroon_path:?}: {err:?}"
            ))
        })?;

        // Convert the bytes to a hex string
        let cert = buffer_as_hex(cert_bytes);
        let tapd_macaroon = buffer_as_hex(tapd_mac_bytes);
        let lnd_macaroon = buffer_as_hex(lnd_mac_bytes);
        let lnd_host = host.unwrap_or(LND_HOST_BOB.to_string());

        Ok(Self {
            asset_id,
            cert,
            tapd_macaroon,
            lnd_macaroon,
            lnd_host,
        })
    }

    // Helper to connect to Tapd and return the connected client
    async fn get_tapd_client(&self) -> Result<lnd_grpc_rust::LndClient, Error> {
        lnd_grpc_rust::connect(
            self.cert.clone(),
            self.tapd_macaroon.clone(),
            self.lnd_host.clone(),
        )
        .await
        .map_err(|err| Error::FailedToGetLnInvoice(format!("Failed to connect to LND: {err:?}")))
    }

    // Helper to connect to LND and return the connected client
    async fn get_lnd_client(&self) -> Result<lnd_grpc_rust::LndClient, Error> {
        lnd_grpc_rust::connect(
            self.cert.clone(),
            self.lnd_macaroon.clone(),
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

        let invoice_request = Some(lnrpc::Invoice {
            memo: memo.into(),
            ..Default::default()
        });
        let request = tapchannelrpc::AddInvoiceRequest {
            asset_id: self.asset_id.clone(),
            asset_amount: amt,
            invoice_request,
            ..Default::default()
        };

        let response = self
            .get_tapd_client()
            .await?
            .tapchannel()
            .add_invoice(request)
            .await
            .map_err(|err| {
                Error::FailedToGetLnInvoice(format!("Failed to add invoice: {err:?}"))
            })?;

        if let Some(invoice_result) = response.into_inner().invoice_result {
            Ok(invoice_result.payment_request)
        } else {
            Err(Error::FailedToGetLnInvoice(
                "Empty invoice returned by LND".to_string(),
            ))
        }
    }

    // Send request to LND node to pay a Taproot asset invoice
    pub async fn pay_asset_invoice(&self, invoice: impl Into<String>) -> Result<i64, Error> {
        let payment_request = lnd_grpc_rust::routerrpc::SendPaymentRequest {
            payment_request: invoice.into(),
            fee_limit_sat: i64::MAX, // TODO: allow user to define limit
            ..Default::default()
        };

        let request = tapchannelrpc::SendPaymentRequest {
            asset_id: self.asset_id.clone(),
            payment_request: Some(payment_request),
            ..Default::default()
        };

        let response = self
            .get_tapd_client()
            .await?
            .tapchannel()
            .send_payment(request)
            .await
            .map_err(|err| {
                Error::FailedToPayLnInvoice(format!("Failed to send payment reqeust: {err:?}"))
            })?;

        let mut stream = response.into_inner();
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(payment_resp) => match payment_resp.result {
                    Some(AcceptedSellOrder(_quote)) => {
                        //println!(">> QUOTE ACCEPTED for amount: {}", quote.asset_amount)
                    }
                    Some(PaymentResult(payment)) => {
                        if let Some(status) = Self::check_payment_final_status(&payment) {
                            return status;
                        }
                    }
                    None => println!(">> NONE IN RESULT"),
                },
                Err(err) => {
                    return Err(Error::FailedToPayLnInvoice(format!(
                        "Failed to get response from LND response stream: {err:?}"
                    )));
                }
            }
        }

        Err(Error::FailedToPayLnInvoice(
            "Payment failed to be processed".to_string(),
        ))
    }

    // Send request to LND node to pay an invoice
    pub async fn pay_invoice(&self, invoice: impl Into<String>) -> Result<i64, Error> {
        let payment_request = lnd_grpc_rust::routerrpc::SendPaymentRequest {
            payment_request: invoice.into(),
            fee_limit_sat: i64::MAX, // TODO: allow user to define limit
            ..Default::default()
        };

        let response = self
            .get_lnd_client()
            .await?
            .router()
            .send_payment_v2(payment_request)
            .await
            .map_err(|err| {
                Error::FailedToPayLnInvoice(format!("Failed to send payment reqeust: {err:?}"))
            })?;

        let mut stream = response.into_inner();
        while let Some(msg) = stream.next().await {
            match msg {
                Ok(payment) => {
                    if let Some(status) = Self::check_payment_final_status(&payment) {
                        return status;
                    }
                }
                Err(err) => {
                    return Err(Error::FailedToPayLnInvoice(format!(
                        "Failed to get response from LND response stream: {err:?}"
                    )));
                }
            }
        }

        Err(Error::FailedToPayLnInvoice(
            "Payment failed to be processed".to_string(),
        ))
    }

    // Helper to check payment response status
    fn check_payment_final_status(payment: &Payment) -> Option<Result<i64, Error>> {
        match payment.status() {
            PaymentStatus::Succeeded => Some(Ok(payment.value_sat)),
            PaymentStatus::Failed => {
                let reason = match payment.failure_reason {
                    val if val == PaymentFailureReason::FailureReasonNone as i32 => {
                        "Payment isn't failed (yet)."
                    }
                    val if val == PaymentFailureReason::FailureReasonTimeout as i32 => {
                        "There are more routes to try, but the payment timeout was exceeded."
                    }
                    val if val == PaymentFailureReason::FailureReasonNoRoute as i32 => {
                        "All possible routes were tried and failed permanently. Or were no routes to the destination at all."
                    }
                    val if val == PaymentFailureReason::FailureReasonError as i32 => {
                        "A non-recoverable error has occured."
                    }
                    val if val
                        == PaymentFailureReason::FailureReasonIncorrectPaymentDetails as i32 =>
                    {
                        "Payment details incorrect (unknown hash, invalid amt or invalid final cltv delta)"
                    }
                    val if val == PaymentFailureReason::FailureReasonInsufficientBalance as i32 => {
                        "Insufficient local balance."
                    }
                    val if val == PaymentFailureReason::FailureReasonCanceled as i32 => {
                        "The payment was canceled."
                    }
                    _ => "unknown",
                };

                Some(Err(Error::FailedToPayLnInvoice(format!(
                    "Payment failed: {reason}"
                ))))
            }
            _status => None,
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
        let ln_client = LnClient::new(None, None).expect("cannot create LnClient");

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

    #[tokio::test]
    async fn test_ln_client_pay_invoice_with_asset_channel() {
        let payee_ln_client = LnClient::new(None, None).expect("cannot create LnClient");
        let amt = Amount::from(2);
        let invoice = payee_ln_client
            .add_invoice("invoice", amt)
            .await
            .expect("couldn't add invoice");
        println!(">>> INVOICE for {amt}: {invoice}");
        assert!(!invoice.is_empty());

        let payer_ln_client = LnClient::new(
            Some(Path::new(TAPROOT_ASSETS_DIR_ALICE)),
            Some(LND_HOST_ALICE.to_string()),
        )
        .expect("cannot create LnClient");

        payer_ln_client
            .pay_asset_invoice(invoice)
            .await
            .expect("cannot pay");
    }

    #[tokio::test]
    async fn test_ln_client_pay_invoice_through_exchange() {
        let payee_ln_client = LnClient::new(None, None).expect("cannot create LnClient");
        let amt = Amount::from(1);
        let invoice = payee_ln_client
            .add_invoice("invoice-edge-node", amt)
            .await
            .expect("couldn't add invoice");
        println!(">>> INVOICE for {amt}: {invoice}");
        assert!(!invoice.is_empty());

        let payer_ln_client = LnClient::new(
            Some(Path::new(TAPROOT_ASSETS_DIR_CAROL)),
            Some(LND_HOST_CAROL.to_string()),
        )
        .expect("cannot create LnClient");

        payer_ln_client
            .pay_invoice(invoice)
            .await
            .expect("cannot pay");
    }
}
