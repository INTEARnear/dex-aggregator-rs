mod common;

use std::time::Duration;

use common::{Router, near_to_usdc, options, outcome, quote_and_fee_transactions, within_a_second};
use dex_aggregator_rs::{
    ConnectionClosed, ConnectionStatus, CryptoHash, OutcomeError, OutcomeWaitUntil,
    RouterConnectionOptions,
};
use serde_json::{Value, json};

fn tx_hash() -> CryptoHash {
    "7xEDD8Bdu9arQn7dDghTDMzhvuwmiKsRFJovq8LyfVQw"
        .parse()
        .unwrap()
}

#[tokio::test]
async fn leaves_out_quotes_whose_transactions_arent_safe_to_sign() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let mut subscription = connection.subscribe(near_to_usdc()).unwrap();
    let mut client = router.accept().await;
    let request = client.receive().await;
    assert_eq!(request["fees_in_next_steps"], true);
    let query = request["query"].as_str().unwrap();
    assert!(
        query.starts_with("token_in=near&token_out=nep141%3A17208628"),
        "{query}"
    );
    assert!(query.contains("&trader_account_id=intear.near&"), "{query}");

    let (safe, _) = quote_and_fee_transactions("near_to_usdc_exact_in", "RheaDcl");
    let mut unsafe_quote = safe.clone();
    unsafe_quote["execution_instructions"][0]["NearTransaction"]["receiver_id"] =
        "evil.near".into();
    let malformed = json!({ "dex_id": "Rhea" });
    client
        .send(json!({ "id": request["id"], "routes": [safe, unsafe_quote, malformed] }))
        .await;
    let update = within_a_second(subscription.next()).await.unwrap().unwrap();
    assert_eq!(update.quotes.len(), 1);
    assert_eq!(update.quotes[0].quote().dex_id, "RheaDcl");
    let rejected = update
        .rejected
        .iter()
        .map(|rejected| rejected.error.problems[0].as_str())
        .collect::<Vec<_>>();
    assert!(rejected[0].starts_with("evil.near near_deposit: Isn't one of the swap's tokens"));
    assert!(
        rejected[1].starts_with("The quote isn't shaped like the router's quotes: missing field")
    );
    assert_eq!(update.rejected[1].json.get(), r#"{"dex_id":"Rhea"}"#);
}

#[tokio::test]
async fn goes_on_routing_and_asks_for_outcomes_again_after_the_router_restarts() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let mut status = connection.status();
    let _subscription = connection.subscribe(near_to_usdc()).unwrap();
    let tx_hashes = [tx_hash()];
    let asking = connection.outcome(7, &tx_hashes, OutcomeWaitUntil::Executed);
    let answering = async {
        let mut client = router.accept().await;
        let route_request = client.receive().await;
        let outcome_request = client.receive().await;
        assert_eq!(
            outcome_request["outcome"],
            json!({ "quote_id": 7, "tx_hashes": [tx_hash().to_string()], "wait_until": "executed" })
        );
        drop(client);

        let mut client = router.accept().await;
        let mut requests = [client.receive().await, client.receive().await];
        requests.sort_by_key(|request| request["id"].as_u64());
        assert_eq!(requests, [route_request, outcome_request.clone()]);
        client
            .send(json!({ "id": outcome_request["id"], "outcome": outcome(vec![]) }))
            .await;
        client
    };
    let (outcome, _client) = within_a_second(async { tokio::join!(asking, answering) }).await;
    assert_eq!(outcome.unwrap().amount_out, 2_846_353);
    assert_eq!(*status.borrow_and_update(), ConnectionStatus::Open);
}

#[tokio::test]
async fn reconnects_after_attempts_that_failed() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    // Nothing accepts the first attempts
    drop(listener);
    let (connection, driver) = dex_aggregator_rs::RouterConnection::new(options(url.clone()));
    tokio::spawn(driver);
    let mut status = connection.status();
    within_a_second(
        status.wait_for(|status| matches!(status, ConnectionStatus::Reconnecting { .. })),
    )
    .await
    .unwrap();
    let listener = tokio::net::TcpListener::bind(url.trim_start_matches("ws://"))
        .await
        .unwrap();
    let (stream, _) = within_a_second(listener.accept()).await.unwrap();
    let _client = tokio_tungstenite::accept_async(stream).await.unwrap();
    within_a_second(status.wait_for(|status| *status == ConnectionStatus::Open))
        .await
        .unwrap();
}

#[tokio::test]
async fn ignores_the_routes_of_a_route_request_it_replaced() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let mut replaced = connection.subscribe(near_to_usdc()).unwrap();
    let mut client = router.accept().await;
    let replaced_request = client.receive().await;
    let mut subscription = connection.subscribe(near_to_usdc()).unwrap();
    let request = client.receive().await;
    assert!(within_a_second(replaced.next()).await.is_none());
    client
        .send(json!({ "id": replaced_request["id"], "routes": [] }))
        .await;
    client
        .send(json!({ "id": request["id"], "routes": [] }))
        .await;
    let update = within_a_second(subscription.next()).await.unwrap().unwrap();
    assert!(update.quotes.is_empty() && update.rejected.is_empty());
}

#[tokio::test]
async fn reports_what_the_router_sends_that_isnt_a_message() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let mut subscription = connection.subscribe(near_to_usdc()).unwrap();
    let mut client = router.accept().await;
    client.receive().await;
    client.send_text("Internal Server Error").await;
    let error = within_a_second(subscription.next())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .0
            .starts_with("The router sent something else than a message"),
        "{error}"
    );
    assert!(error.0.ends_with(": Internal Server Error"), "{error}");
}

#[tokio::test]
async fn asks_for_the_routes_again_after_the_routers_errors() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let mut subscription = connection.subscribe(near_to_usdc()).unwrap();
    let mut client = router.accept().await;
    let request = client.receive().await;
    client
        .send(json!({ "id": request["id"], "error": "Rate limited" }))
        .await;
    let error = within_a_second(subscription.next())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.0, "Rate limited");
    assert_eq!(client.receive().await, request);
    client
        .send(json!({ "id": request["id"], "routes": [] }))
        .await;
    within_a_second(subscription.next()).await.unwrap().unwrap();
}

#[tokio::test]
async fn dropping_the_subscription_stops_routing() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let subscription = connection.subscribe(near_to_usdc()).unwrap();
    let mut client = router.accept().await;
    let request = client.receive().await;
    drop(subscription);
    let stop = client.receive().await;
    assert!(stop["id"].as_u64() > request["id"].as_u64());
    assert_eq!(stop.as_object().unwrap().len(), 1, "{stop}");
}

#[tokio::test]
async fn asks_for_an_outcome_again_after_the_routers_errors() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let tx_hashes = [tx_hash()];
    let asking = connection.outcome(7, &tx_hashes, OutcomeWaitUntil::Swap);
    let answering = async {
        let mut client = router.accept().await;
        let request = client.receive().await;
        assert_eq!(request["outcome"]["wait_until"], "swap");
        client
            .send(json!({ "id": request["id"], "error": "Transactions didn't execute in 60s" }))
            .await;
        assert_eq!(client.receive().await, request);
        client
            .send(json!({ "id": request["id"], "outcome": outcome(vec![]) }))
            .await;
        client
    };
    let (outcome, _client) = within_a_second(async { tokio::join!(asking, answering) }).await;
    outcome.unwrap();
}

#[tokio::test]
async fn gives_up_on_an_outcome_after_its_timeout_with_the_routers_last_error() {
    let router = Router::new().await;
    let connection = router.connect(RouterConnectionOptions {
        outcome_timeout: Duration::from_millis(300),
        ..options(router.url())
    });
    let tx_hashes = [tx_hash()];
    let asking = connection.outcome(7, &tx_hashes, OutcomeWaitUntil::Executed);
    let answering = async {
        let mut client = router.accept().await;
        loop {
            let request = client.receive().await;
            client
                .send(json!({ "id": request["id"], "error": "Quote 7 doesn't exist" }))
                .await;
        }
    };
    let error = tokio::select! {
        outcome = asking => outcome.unwrap_err(),
        () = answering => unreachable!(),
    };
    assert!(
        matches!(&error, OutcomeError::TimedOut { last_error: Some(last_error), .. } if last_error == "Quote 7 doesn't exist"),
        "{error}"
    );
}

#[tokio::test]
async fn an_outcome_that_isnt_shaped_like_the_routers() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let tx_hashes = [tx_hash()];
    let asking = connection.outcome(7, &tx_hashes, OutcomeWaitUntil::Executed);
    let answering = async {
        let mut client = router.accept().await;
        let request = client.receive().await;
        let mut malformed = outcome(vec![]);
        malformed.as_object_mut().unwrap().remove("next_steps");
        client
            .send(json!({ "id": request["id"], "outcome": malformed }))
            .await;
        client
    };
    let (outcome, _client) = within_a_second(async { tokio::join!(asking, answering) }).await;
    assert!(matches!(outcome.unwrap_err(), OutcomeError::Malformed(_)));
}

#[tokio::test]
async fn closing_fails_the_outcomes_it_waits_for_and_ends_the_subscription() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let mut subscription = connection.subscribe(near_to_usdc()).unwrap();
    let tx_hashes = [tx_hash()];
    let asking = connection.outcome(7, &tx_hashes, OutcomeWaitUntil::Executed);
    let closing = async {
        let mut client = router.accept().await;
        client.receive().await;
        client.receive().await;
        connection.close();
    };
    let (outcome, ()) = within_a_second(async { tokio::join!(asking, closing) }).await;
    assert!(matches!(outcome.unwrap_err(), OutcomeError::Closed));
    assert!(within_a_second(subscription.next()).await.is_none());
    let mut status = connection.status();
    within_a_second(status.wait_for(|status| *status == ConnectionStatus::Closed))
        .await
        .unwrap();
    assert_eq!(
        connection.subscribe(near_to_usdc()).unwrap_err(),
        ConnectionClosed
    );
}

#[tokio::test]
async fn a_route_request_of_every_field() {
    let router = Router::new().await;
    let connection = router.connect(options(router.url()));
    let request = dex_aggregator_rs::SwapRequest {
        signing_public_key: Some(
            "ed25519:7EK1MhudQrXZBZjvnvvcNXiQ2ByYBkXg8rUhbKL69CGo".to_string(),
        ),
        dexes: Some(vec!["RheaDcl".to_string()]),
        slippage: dex_aggregator_rs::Slippage::Auto {
            min: 0.001,
            max: 0.05,
        },
        ..near_to_usdc()
    };
    let _subscription = connection.subscribe(request).unwrap();
    let mut client = router.accept().await;
    let query: Value = client.receive().await["query"].clone();
    assert_eq!(
        query,
        format!(
            "token_in=near&token_out=nep141%3A{}&amount_in=1000000000000000000000000&slippage_type=Auto\
            &min_slippage=0.001&max_slippage=0.05&trader_account_id=intear.near\
            &signing_public_key=ed25519%3A7EK1MhudQrXZBZjvnvvcNXiQ2ByYBkXg8rUhbKL69CGo&dexes=RheaDcl\
            &referrer_id=wallet.intear.near&fees=%7B%22wallet.intear.near%22%3A0.005%7D\
            &quote_improvement_receiver=wallet.intear.near",
            common::USDC
        )
    );
}

#[tokio::test]
async fn connects_again_when_the_router_stops_answering() {
    let router = Router::new().await;
    let connection = router.connect(RouterConnectionOptions {
        ping_interval: Duration::from_millis(100),
        ..options(router.url())
    });
    let _subscription = connection.subscribe(near_to_usdc()).unwrap();
    // Reads the route request, then nothing, so it answers no pings, like a router gone silent
    // without closing the connection
    let mut silent = router.accept().await;
    let request = silent.receive().await;
    let mut client = router.accept().await;
    assert_eq!(client.receive().await, request);
    drop(silent);
}

#[tokio::test]
async fn outcomes_wait_for_a_router_offline_for_longer_than_their_timeout() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let connection = {
        let (connection, driver) =
            dex_aggregator_rs::RouterConnection::new(RouterConnectionOptions {
                outcome_timeout: Duration::from_millis(300),
                ..options(format!("ws://{address}"))
            });
        tokio::spawn(driver);
        connection
    };
    let tx_hashes = [tx_hash()];
    let asking = connection.outcome(7, &tx_hashes, OutcomeWaitUntil::Executed);
    let answering = async {
        let (stream, _) = listener.accept().await.unwrap();
        let mut client =
            common::Client::new(tokio_tungstenite::accept_async(stream).await.unwrap());
        let request = client.receive().await;
        // Offline for longer than the timeout
        drop((client, listener));
        tokio::time::sleep(Duration::from_secs(1)).await;
        let listener = tokio::net::TcpListener::bind(address).await.unwrap();
        let (stream, _) = listener.accept().await.unwrap();
        let mut client =
            common::Client::new(tokio_tungstenite::accept_async(stream).await.unwrap());
        assert_eq!(client.receive().await, request);
        client
            .send(json!({ "id": request["id"], "outcome": outcome(vec![]) }))
            .await;
        client
    };
    let (outcome, _client) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(asking, answering)
    })
    .await
    .unwrap();
    outcome.unwrap();
}
