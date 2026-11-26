#![allow(dead_code, reason = "Each test crate uses some of it")]

use std::{collections::BTreeMap, time::Duration};

use dex_aggregator_rs::{
    RouterConnection, RouterConnectionOptions, Slippage, SwapAmount, SwapRequest,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};

pub const USDC: &str = "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1";

/// What `near_to_usdc_exact_in` was requested with
pub fn near_to_usdc() -> SwapRequest {
    SwapRequest {
        referrer_id: Some("wallet.intear.near".parse().unwrap()),
        fees: BTreeMap::from([("wallet.intear.near".parse().unwrap(), 0.005)]),
        quote_improvement_receiver: Some("wallet.intear.near".parse().unwrap()),
        ..SwapRequest::new(
            "near".parse().unwrap(),
            USDC.parse().unwrap(),
            SwapAmount::ExactIn(10u128.pow(24)),
            Slippage::Fixed(0.01),
            "intear.near".parse().unwrap(),
        )
    }
}

/// A quote of the fixture as a subscription with its fees in the next steps has it, and its fee
/// transactions
pub fn quote_and_fee_transactions(fixture: &str, dex_id: &str) -> (Value, Vec<Value>) {
    let path = format!(
        "{}/tests/fixtures/{fixture}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let quotes: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut quote = quotes
        .into_iter()
        .find(|quote| quote["dex_id"] == dex_id)
        .unwrap();
    let (fee_transactions, transactions) = quote["execution_instructions"]
        .as_array()
        .unwrap()
        .iter()
        .cloned()
        .partition(|instruction| instruction["NearTransaction"]["is_fee_transaction"] == true);
    quote["execution_instructions"] = Value::Array(transactions);
    quote["quote_id"] = json!(7);
    (quote, fee_transactions)
}

pub fn outcome(next_steps: Vec<Value>) -> Value {
    json!({
        "charge_fee": true,
        "amount_in": "995000000000000000000000",
        "amount_out": "2846353",
        "balance_changes": [],
        "next_steps": next_steps,
        "error": null,
        "settled": true,
    })
}

/// Retries and gives up fast
pub fn options(url: String) -> RouterConnectionOptions {
    RouterConnectionOptions {
        url,
        min_retry_delay: Duration::from_millis(10),
        max_retry_delay: Duration::from_millis(40),
        connect_timeout: Duration::from_secs(1),
        outcome_timeout: Duration::from_secs(2),
        // The clients of these routers only read when a test does, so they only answer pings then
        ping_interval: Duration::from_secs(10),
    }
}

/// Answers like the router does, on a port of its own
pub struct Router {
    listener: TcpListener,
}

impl Router {
    pub async fn new() -> Self {
        Self {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
        }
    }

    pub fn url(&self) -> String {
        format!("ws://{}", self.listener.local_addr().unwrap())
    }

    /// With the connection's driver spawned
    pub fn connect(&self, options: RouterConnectionOptions) -> RouterConnection {
        let (connection, driver) = RouterConnection::new(options);
        tokio::spawn(driver);
        connection
    }

    pub async fn accept(&self) -> Client {
        let (stream, _) = within_a_second(self.listener.accept()).await.unwrap();
        Client(tokio_tungstenite::accept_async(stream).await.unwrap())
    }
}

pub struct Client(WebSocketStream<TcpStream>);

impl Client {
    pub fn new(socket: WebSocketStream<TcpStream>) -> Self {
        Self(socket)
    }

    pub async fn receive(&mut self) -> Value {
        match within_a_second(self.0.next()).await {
            Some(Ok(Message::Text(text))) => serde_json::from_str(&text).unwrap(),
            message => panic!("Received {message:?} instead of a message"),
        }
    }

    pub async fn send(&mut self, message: Value) {
        self.0
            .send(Message::text(message.to_string()))
            .await
            .unwrap();
    }

    pub async fn send_text(&mut self, text: &str) {
        self.0.send(Message::text(text)).await.unwrap();
    }
}

pub async fn within_a_second<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(1), future)
        .await
        .expect("Didn't happen in a second")
}
