mod common;

use std::sync::{Arc, Mutex};

use base64::{Engine, prelude::BASE64_STANDARD};
use common::{
    Router, USDC, near_to_usdc, options, outcome, quote_and_fee_transactions, within_a_second,
};
use dex_aggregator_rs::{
    CheckedQuote, CryptoHash, ExecutionError, NextStepsError, Quote, SwapRequest, Transaction,
    TransactionSender,
};
use serde_json::json;

/// Signs as `intear.near`, failing on the transaction of `fails_at`
#[derive(Default)]
struct Sender {
    account_id: &'static str,
    sent: Mutex<Vec<Transaction>>,
    fails_at: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
#[error("The RPC is down")]
struct RpcDown;

impl TransactionSender for Sender {
    type Error = RpcDown;

    fn account_id(&self) -> &str {
        self.account_id
    }

    async fn send_transaction(&self, transaction: Transaction) -> Result<CryptoHash, RpcDown> {
        let mut sent = self.sent.lock().unwrap();
        if self.fails_at == Some(sent.len()) {
            return Err(RpcDown);
        }
        sent.push(transaction);
        Ok(CryptoHash([sent.len() as u8; 32]))
    }
}

fn trader() -> Sender {
    Sender {
        account_id: "intear.near",
        ..Sender::default()
    }
}

fn checked_quote(request: SwapRequest, fixture: &str, dex_id: &str) -> CheckedQuote {
    let (quote, _) = quote_and_fee_transactions(fixture, dex_id);
    let quote: Quote = serde_json::from_value(quote).unwrap();
    CheckedQuote::new(Arc::new(request), quote).unwrap()
}

#[tokio::test]
async fn swapping_sends_the_quotes_transactions_then_its_next_steps() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let sender = trader();
    let (_, fee_transactions) = quote_and_fee_transactions("near_to_usdc_exact_in", "RheaDcl");
    let executed = checked_quote(near_to_usdc(), "near_to_usdc_exact_in", "RheaDcl")
        .execute(&sender)
        .await
        .unwrap();
    assert_eq!(executed.tx_hashes(), [CryptoHash([1; 32])]);

    let asking = executed.next_steps(&connection);
    let answering = async {
        let mut client = router.accept().await;
        let request = client.receive().await;
        assert_eq!(
            request["outcome"],
            json!({ "quote_id": 7, "tx_hashes": [CryptoHash([1; 32]).to_string()], "wait_until": "executed" })
        );
        client
            .send(json!({ "id": request["id"], "outcome": outcome(fee_transactions) }))
            .await;
        client
    };
    let (next_steps, _client) = within_a_second(async { tokio::join!(asking, answering) }).await;
    let next_steps = next_steps.unwrap();
    assert!(next_steps.outcome().charge_fee);
    assert_eq!(
        next_steps.execute(&sender).await.unwrap(),
        [CryptoHash([2; 32])]
    );

    let sent = sender.sent.into_inner().unwrap();
    let methods = sent
        .iter()
        .map(|transaction| {
            let methods = transaction
                .actions
                .iter()
                .map(|call| call.method_name.as_str());
            (
                transaction.receiver_id.as_str(),
                methods.collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        methods,
        [
            ("wrap.near", vec!["near_deposit", "ft_transfer_call"]),
            ("wrap.near", vec!["ft_transfer", "ft_transfer"]),
        ]
    );
}

#[tokio::test]
async fn unsafe_next_steps_arent_signed() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let executed = checked_quote(near_to_usdc(), "near_to_usdc_exact_in", "RheaDcl")
        .execute(&trader())
        .await
        .unwrap();
    let transfer_away = json!({ "NearTransaction": {
        "receiver_id": "wrap.near",
        "actions": [{ "FunctionCall": {
            "method_name": "ft_transfer",
            "args": BASE64_STANDARD.encode(r#"{"receiver_id":"evil.near","amount":"1","memo":"Trading fee (evil.near)"}"#),
            "gas": 20_000_000_000_000u64,
            "deposit": "1",
        } }],
        "is_fee_transaction": true,
    } });
    let asking = executed.next_steps(&connection);
    let answering = async {
        let mut client = router.accept().await;
        let request = client.receive().await;
        client
            .send(json!({ "id": request["id"], "outcome": outcome(vec![transfer_away]) }))
            .await;
        client
    };
    let (next_steps, _client) = within_a_second(async { tokio::join!(asking, answering) }).await;
    let Err(NextStepsError::Unsafe(error)) = next_steps else {
        panic!("{next_steps:?} isn't unsafe");
    };
    assert_eq!(
        error.problems,
        ["wrap.near ft_transfer: Pays a fee to evil.near, which the request doesn't"]
    );
}

#[tokio::test]
async fn a_sender_that_fails_on_a_transaction_tells_the_ones_that_executed_before_it() {
    let sender = Sender {
        fails_at: Some(1),
        ..trader()
    };
    let request = SwapRequest {
        token_in: "rhea-nep141:wrap.near".parse().unwrap(),
        token_out: format!("rhea-nep141:{USDC}").parse().unwrap(),
        ..near_to_usdc()
    };
    // Withdraws from Rhea, then swaps
    let checked = checked_quote(request, "rhea_balances_exact_in", "RheaDcl");
    assert_eq!(checked.transactions().len(), 2);
    let Err(ExecutionError::Interrupted { tx_hashes, error }) = checked.execute(&sender).await
    else {
        panic!("Didn't stop");
    };
    assert_eq!(tx_hashes, [CryptoHash([1; 32])]);
    assert_eq!(error.to_string(), "The RPC is down");
}

#[tokio::test]
async fn a_sender_of_another_account_sends_nothing() {
    let sender = Sender {
        account_id: "bob.near",
        ..Sender::default()
    };
    let error = checked_quote(near_to_usdc(), "near_to_usdc_exact_in", "Rhea")
        .execute(&sender)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "The sender signs as bob.near, not as the trader intear.near"
    );
    assert!(sender.sent.into_inner().unwrap().is_empty());
}
