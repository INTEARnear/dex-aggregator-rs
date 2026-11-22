use std::collections::BTreeMap;

use dex_aggregator_rs::{
    RouterAmount, RouterConnection, RouterConnectionOptions, Slippage, SwapAmount, SwapRequest,
};

const USDC: &str = "17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let trader = near_kit::Near::mainnet()
        .build()
        .with_signer(near_kit::signer::from_env()?);
    let request = SwapRequest {
        fees: BTreeMap::from([("app.near".parse()?, 0.005)]),
        quote_improvement_receiver: Some("app.near".parse()?),
        ..SwapRequest::new(
            "near".parse()?,
            USDC.parse()?,
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
    let tx_hashes = next_steps.execute(&trader).await?;
    println!("Paid the fees in {tx_hashes:?}");
    Ok(())
}
