use cognitoauth::cognito_srp_auth::{auth, CognitoAuthInput};
use ethers::{
    providers::ProviderError,
    types::{
        transaction::eip2718::TypedTransaction, Bytes, NameOrAddress, TransactionReceipt, TxHash,
        U256, U64,
    },
};
use hyper::StatusCode;
use once_cell::sync::Lazy;
use prometheus::{register_int_counter_vec, IntCounterVec};
use reqwest::{header::HeaderValue, Client};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::HashMap, fmt::Debug, time::Duration};
use thiserror::Error;
use tokio::{sync::Mutex, time::timeout};
use tracing::{error, info, info_span, Instrument};

use crate::ethereum::TxError;

// Same for every project, taken from here: https://docs.openzeppelin.com/defender/api-auth
const RELAY_TXS_URL: &str = "https://api.defender.openzeppelin.com/txs";
const CLIENT_ID: &str = "1bpd19lcr33qvg5cr3oi79rdap";
const POOL_ID: &str = "us-west-2_iLmIggsiy";

static CLIENTS: Lazy<Mutex<HashMap<String, ExpiringClient>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

static TX_COUNT: Lazy<IntCounterVec> = Lazy::new(|| {
    register_int_counter_vec!("eth_tx_count", "The transaction count by bytes4.", &[
        "bytes4"
    ])
    .unwrap()
});

#[derive(Clone, Debug)]
pub struct OzRelay {
    api_key:      String,
    api_secret:   String,
    send_timeout: Duration,
}

impl OzRelay {
    pub fn new(api_key: &str, api_secret: &str) -> Self {
        Self {
            api_key:      api_key.to_string(),
            api_secret:   api_secret.to_string(),
            send_timeout: Duration::from_secs(60),
        }
    }

    async fn query(&self, tx_id: &str) -> Result<SubmittedTransaction, Error> {
        let url = format!("{RELAY_TXS_URL}/{tx_id}");
        let client = get_client(&self.api_key, &self.api_secret)
            .await
            .map_err(|_| Error::Authentication)?;

        let res = client
            .get(url)
            .send()
            .await
            .map_err(|_| Error::Authentication)?;

        let status = res.status();
        let item = res.json::<SubmittedTransaction>().await.map_err(|e| {
            error!(?e, "error occurred");
            Error::UnknownResponse
        })?;

        info!(?status, ?item, "query response");

        Ok(item)
    }

    async fn list_transactions(&self) -> Result<Vec<SubmittedTransaction>, Error> {
        let client = get_client(&self.api_key, &self.api_secret)
            .await
            .map_err(|_| Error::Authentication)?;

        let res = client
            .get(RELAY_TXS_URL)
            .send()
            .await
            .map_err(|_| Error::Authentication)?;

        let status = res.status();
        let items = res.json::<Vec<SubmittedTransaction>>().await.map_err(|e| {
            error!(?e, "error occurred");
            Error::UnknownResponse
        })?;

        info!(?status, ?items, "list response");

        Ok(items)
    }

    async fn mine_transaction_id(&self, id: &str) -> Result<SubmittedTransaction, TxError> {
        loop {
            let transaction = self.query(id).await.map_err(|error| {
                error!(?error, "Failed to get transaction status");
                TxError::Send(Box::new(error))
            })?;
            let status = transaction
                .status
                .as_ref()
                .ok_or_else(|| TxError::Dropped(TxHash::default()))?;

            if status == "mined" || status == "confirmed" || status == "failed" {
                return Ok(transaction);
            }

            info!("waiting 5 s to mine");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn send_oz_transaction<T: Into<TypedTransaction> + Send + Sync>(
        &self,
        tx: T,
    ) -> Result<String, Error> {
        let client = get_client(&self.api_key, &self.api_secret)
            .await
            .map_err(|_| Error::Authentication)?;

        let tx: TypedTransaction = tx.into();
        let api_tx = Transaction {
            to:        tx.to(),
            value:     tx.value(),
            gas_limit: tx.gas(),
            data:      tx.data(),
        };

        let res = client
            .post(RELAY_TXS_URL)
            .body(json!(api_tx).to_string())
            .send()
            .await
            .map_err(|_| Error::Authentication)?;

        if res.status() == StatusCode::OK {
            let obj = res
                .json::<Value>()
                .await
                .map_err(|_| Error::UnknownResponse)?;
            let id = obj
                .get("transactionId")
                .ok_or(Error::UnknownResponse)?
                .as_str()
                .unwrap();
            Ok(id.to_string())
        } else {
            info!(?res, "response status");
            let text = res.text().await;
            info!(?text, "response error");

            Err(Error::Authentication)
        }
    }

    pub async fn send_transaction(
        &self,
        tx: TypedTransaction,
        is_retry: bool,
    ) -> Result<TransactionReceipt, TxError> {
        let mut tx = tx.clone();
        tx.set_gas(1_000_000);

        if is_retry {
            info!(is_retry, "checking if can resubmit");

            let existing_transactions = self.list_transactions().await.map_err(|e| {
                error!(?e, "error occurred");
                TxError::Send(Box::new(e))
            })?;

            let existing_transaction =
                existing_transactions
                    .iter()
                    .find(|el| match (&el.data, tx.data()) {
                        (Some(a), Some(b)) => a == b,
                        _ => false,
                    });

            if let Some(existing_transaction) = existing_transaction {
                self.mine_transaction_id(existing_transaction.transaction_id.as_ref().unwrap())
                    .await?;

                // TODO: return something meaningful
                return Ok(TransactionReceipt {
                    block_number: Some(U64::from(10)),
                    ..Default::default()
                });
            }
        }

        info!(?tx, gas_limit=?tx.gas(), "Sending transaction.");
        let bytes4: u32 = tx.data().map_or(0, |data| {
            let mut buffer = [0; 4];
            buffer.copy_from_slice(&data.as_ref()[..4]); // TODO: Don't panic.
            u32::from_be_bytes(buffer)
        });
        let bytes4 = format!("{bytes4:8x}");
        TX_COUNT.with_label_values(&[&bytes4]).inc();

        // Send TX to OZ Relay
        let tx_id = timeout(self.send_timeout, self.send_oz_transaction(tx.clone()))
            .instrument(info_span!("Send TX to mempool"))
            .await
            .map_err(|elapsed| {
                error!(?elapsed, "Send transaction timed out");
                TxError::SendTimeout
            })?
            .map_err(|error| {
                error!(?error, "Failed to send transaction");
                TxError::Send(Box::new(error))
            })?;

        info!(?tx_id, "Transaction submitted to OZ Relay");

        self.mine_transaction_id(&tx_id).await?;

        // TODO: return something meaningful
        Ok(TransactionReceipt {
            block_number: Some(U64::from(10)),
            ..Default::default()
        })
    }
}

#[derive(Debug)]
struct ExpiringClient {
    client:          Client,
    expiration_time: i64,
}

/// Refreshes or creates a new access token for Defender API and returns it.
async fn get_client(api_key: &str, api_secret: &str) -> eyre::Result<Client> {
    let now = chrono::Utc::now().timestamp();
    let mut clients = CLIENTS.lock().await;
    if let Some(client) = clients.get(api_key) {
        if now < client.expiration_time {
            // token still valid
            return Ok(client.client.clone());
        }
    }

    let input = CognitoAuthInput {
        client_id:     CLIENT_ID.to_string(),
        pool_id:       POOL_ID.to_string(),
        username:      api_key.to_string(),
        password:      api_secret.to_string(),
        mfa:           None,
        client_secret: None,
    };

    let res = auth(input)
        .await
        .map_err(|_| eyre::eyre!("Authentication failed"))?
        .ok_or(eyre::eyre!("Authentication failed"))?;

    let access_token = res
        .access_token()
        .ok_or(eyre::eyre!("Authentication failed"))?;

    let mut auth_value = HeaderValue::from_str(access_token)?;
    auth_value.set_sensitive(true);

    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::AUTHORIZATION, auth_value);
    headers.insert("X-Api-Key", HeaderValue::from_str(api_key)?);

    let client = Client::builder().default_headers(headers).build()?;

    clients.insert(api_key.to_string(), ExpiringClient {
        client:          client.clone(),
        expiration_time: now + i64::from(res.expires_in()),
    });
    Ok(client)
}

#[derive(Error, Debug)]
pub enum Error {
    #[error("Transport error")]
    Transport(#[from] ethers::providers::HttpClientError),
    #[error("Authentication error")]
    Authentication,
    #[error("Unknown response")]
    UnknownResponse,
}

impl From<Error> for ProviderError {
    fn from(error: Error) -> Self {
        Self::JsonRpcClientError(Box::new(error))
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to:        Option<&'a NameOrAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value:     Option<&'a U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_limit: Option<&'a U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data:      Option<&'a Bytes>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmittedTransaction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to:             Option<NameOrAddress>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value:          Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gas_limit:      Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data:           Option<Bytes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status:         Option<String>,
}
