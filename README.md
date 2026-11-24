# dex-aggregator-rs

> [!WARNING]
> 
> This crate was not fully audited yet.

Client of the Intear DEX Aggregator (router.intear.tech) for accounts that own the signer, like bots and wallets, that checks the router's transactions before they're signed.

## Usage

Add `dex-aggregator-rs` to your `Cargo.toml`:

```toml
[dependencies]
dex-aggregator-rs = { git = "https://github.com/INTEARnear/dex-aggregator-rs" }
```

1. `RouterConnection::new` gives the connection and the future that keeps it, to spawn.
2. `subscribe` gives the quotes of a request, again whenever the pools change, as `CheckedQuote`s, best first, and the quotes whose transactions aren't safe to sign, with why.
3. `CheckedQuote::execute` sends the quote's transactions with a `TransactionSender`, one after another.
4. `ExecutedSwap::next_steps` asks the router what the swap did, and checks the next steps of its outcome.
5. `NextSteps::execute` sends them: they move the tokens where the trader asked for them, and, once the swap went through, pay the fees and the quote improvement fee.

```rust
use std::collections::BTreeMap;

use dex_aggregator_rs::{
    RouterAmount, RouterConnection, RouterConnectionOptions, Slippage, SwapAmount, SwapRequest,
};

let trader = near_kit::Near::mainnet()
    .build()
    .with_signer(near_kit::signer::from_env()?);
let request = SwapRequest {
    fees: BTreeMap::from([("app.near".parse()?, 0.005)]),
    quote_improvement_receiver: Some("app.near".parse()?),
    ..SwapRequest::new(
        "near".parse()?,
        "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1".parse()?, // USDC
        SwapAmount::ExactIn(10u128.pow(24)),
        Slippage::Fixed(0.01),
        trader.account_id().clone(),
    )
};
// The router estimates the price, so the least to take for 1 NEAR comes from a price you trust
let least_amount_out = 2_500_000; // 2.5 USDC

let (connection, driver) = RouterConnection::new(RouterConnectionOptions::default());
tokio::spawn(driver);
let mut quotes = connection.subscribe(request)?;
let best = loop {
    match quotes.next().await {
        Some(Ok(update)) => {
            // `update.rejected` has the quotes whose transactions aren't safe to sign, and why
            let best = update.quotes.into_iter().next().filter(|best| {
                matches!(best.quote().worst_case_amount, RouterAmount::AmountOut(amount) if amount >= least_amount_out)
            });
            if let Some(best) = best {
                break best;
            }
        }
        // The route request is sent again after it
        Some(Err(error)) => eprintln!("The router failed: {error}"),
        None => return Err("The connection to the router closed".into()),
    }
};
drop(quotes);

let swap = best.execute(&trader).await?;
println!("Swapped in {:?}", swap.tx_hashes());
let next_steps = swap.next_steps(&connection).await?;
println!("{:?}", next_steps.outcome());
next_steps.execute(&trader).await?;
```

It's [`examples/swap.rs`](examples/swap.rs): `cargo run --example swap --features near-kit`, with `NEAR_ACCOUNT_ID` and `NEAR_PRIVATE_KEY` set.

## Senders

A `TransactionSender` signs a transaction, sends it, and returns its hash once it executed, also when a receipt of it failed: the router tells what the transactions did, and the next steps return what a failed swap left behind. It has to wait for the execution, as the router's transactions after the first rely on what the ones before them did. Its `account_id` has to be the request's `trader_account_id`, or nothing is sent.

Each library has its own feature:

| Feature | Sender |
| --- | --- |
| `near-api` | `adapters::near_api::NearApiSender { account_id, signer, network }` |
| `near-kit` | `near_kit::SigningNear` itself, whatever its wait level |

They wait until a transaction executed optimistically.

When `send_transaction` fails, `execute` stops with `ExecutionError::Interrupted`, with the hashes of the transactions that executed before it.

## What's checked

`CheckedQuote::new` (on every quote of a subscription) and `ExecutedSwap::next_steps` only accept the calls the router makes, and read the arguments they check typed and by name: one of them that is there twice or is of another type, or arguments in an array instead of an object, which a contract would read by position, make them unsafe. Intear DEX's operations are read with its own types, of [`intear-dex-types`](https://github.com/INTEARnear/dex), as the engine reads them. They check that:

- the quote has no fee transactions, and its transactions spend at most the exact input, or the quote's worst case of an exact-out swap, with the quote's `fee_amount` that its next steps pay, and wrap at most that much NEAR
- the quote's `fee_amount`, the fees paid on Rhea and the referral fees that Intear DEX takes add up to no more than the requested fees
- the swaps give at least the quote's worst case of exact-in, which is within the requested slippage of its estimate, or the requested output of exact-out, which only swaps on Rhea DCL and Intear DEX, whose swaps give exactly what they're asked for
- they only call the swap's tokens, wNEAR, Rhea, Rhea DCL and Intear DEX, and only send the trader's tokens to those DEXes and to the requested fee receivers
- Rhea and Rhea DCL are called with exactly the arguments that routes pass, as what others of theirs do isn't known to the checks
- Intear DEX's operations only swap the input for the output, an amount of it and not a whole balance, also through other tokens one swap after another or split into parts, withdraw to the trader, move the trader's assets with none of the engine's other operations, like transfers, calls to its dexes and storage deposits, pay oracles no more than `MAX_ORACLE_FEES` (1 NEAR, 1 USDT, 1 USDC or 1000 TEAR of the input, out of it or by a swap that buys their fee, and no other asset), and take the requested referral fees and a quote improvement fee for the requested receiver of no more than the quote's estimate beats, which split swaps take of what each beats its part of the estimate by
- storage deposits attach no more than the quote's `storage_deposits`, and no more than `MAX_STORAGE_DEPOSITS` (1 NEAR) in all, also in the next steps, for the trader, the DEXes, the fee receivers and the referrer, and leave no more than `MAX_STORAGE_DEPOSIT_FOR_OTHERS` (0.05 NEAR) on Rhea to each account other than the trader, which it could withdraw
- next steps don't swap, only pay fees after a swap that went through, no more than the quote's `fee_amount`, and pay the quote improvement fee, a plain transfer of the output for exact-in and of the input for exact-out, that leaves the trader with exactly the quote's estimate: the outcome's `amount_out` of exact-in, or `amount_in` of exact-out, which are what the trader got and spent, fees included
- there are at most 20 transactions, as many as the router finds the outcome of

Fields and operations that none of this is about are left to the router, so that it can add them: arguments of Intear DEX and of tokens that the checks don't read, rescue addresses, which only get what Intear DEX can't send to the trader, and Intear DEX's operations that move none of the trader's assets, like registering assets.

A check that fails gives an `UnsafeTransactionsError`, with every problem in `problems`. A quote that isn't shaped like the router's quotes is rejected with the reason, and an outcome that isn't shaped like its outcomes fails with `OutcomeError::Malformed`. Amounts are added up without overflowing, as release builds don't check it.

The quote's estimate and worst case come from the router, so the checks can't tell a bad price: compare them with a price you trust before swapping.

## When the router goes offline

The connection reconnects whenever it's lost, waiting twice as long after each failed attempt (250 ms to 10 s by default), and gives up on an attempt after `connect_timeout`. Once it's back, it sends the route request again, and every outcome request that wasn't answered, which the router answers from the quotes it stored by their ID, also after a restart. `status()` starts as `Connecting`, then is `Open`, `Reconnecting` with the error, and `Closed`. Right after a restart, the router can answer with no routes until it has loaded the pools.

A router that goes offline without closing the connection, or a network between them that goes down, doesn't tell: natively, the connection pings the router once it's been quiet for `ping_interval` (10 s by default), and connects again when the router doesn't answer in as long.

A route request or an outcome request that the router answers with an error is sent again after the same delays. An outcome is asked for until the router hasn't told it for `outcome_timeout` (3 minutes by default) on one connection, when it fails with the router's last error: while the router is offline, outcomes wait for it, and each connection gives it that long again. So a swap goes through however long the router is offline, and however often, once it's back: put a timeout of your own around `next_steps` to give up on it. The subscription gives the router's errors, and goes on.

Dropping the `QuoteSubscription` stops the route request, a new subscription replaces it, and the connection closes once `close` is called or every clone of it is dropped, failing the outcomes it waits for.
