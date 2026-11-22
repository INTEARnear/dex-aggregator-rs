use std::sync::Arc;

use near_account_id::AccountId;

use crate::{
    connection::{OutcomeError, RouterConnection},
    request::SwapRequest,
    sender::{Transaction, TransactionSender},
    types::{CryptoHash, OutcomeWaitUntil, Quote, SwapOutcome},
    validate::{self, UnsafeTransactionsError},
};

/// A quote whose transactions only do what the request asks, see [`CheckedQuote::new`]
#[derive(Debug, Clone)]
pub struct CheckedQuote {
    request: Arc<SwapRequest>,
    quote: Quote,
    transactions: Vec<Transaction>,
}

impl CheckedQuote {
    /// Checks that the quote's transactions spend at most the input the trader allows (the exact
    /// input, or the quote's worst case of exact-out) with the fees its next steps pay, get at
    /// least the quote's worst case of the output, call only the tokens of the swap and the DEXes
    /// the router uses, attach no more to storage deposits than the quote says and the limits
    /// allow, and take fees only for the requested receivers, no more than requested.
    ///
    /// The quote's estimate and worst case come from the router, so the checks can't tell a bad
    /// price: compare them with a price you trust before swapping.
    pub fn new(request: Arc<SwapRequest>, quote: Quote) -> Result<Self, UnsafeTransactionsError> {
        let transactions = validate::check_quote(&request, &quote)?;
        Ok(Self {
            request,
            quote,
            transactions,
        })
    }

    pub fn quote(&self) -> &Quote {
        &self.quote
    }

    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    /// Sends the quote's transactions, one after another
    pub async fn execute<S: TransactionSender>(
        self,
        sender: &S,
    ) -> Result<ExecutedSwap, ExecutionError<S::Error>> {
        let tx_hashes =
            send_in_order(sender, &self.request.trader_account_id, self.transactions).await?;
        Ok(ExecutedSwap {
            request: self.request,
            quote: self.quote,
            tx_hashes,
        })
    }
}

/// The quote's transactions executed
#[derive(Debug, Clone)]
pub struct ExecutedSwap {
    request: Arc<SwapRequest>,
    quote: Quote,
    tx_hashes: Vec<CryptoHash>,
}

impl ExecutedSwap {
    /// Tell what the swap did when the router doesn't
    pub fn tx_hashes(&self) -> &[CryptoHash] {
        &self.tx_hashes
    }

    /// Asks the router what the swap did, and checks that the next steps of its outcome only move
    /// the trader's input and output between the places they hold them, and, after a swap that
    /// went through, pay its fees and the quote improvement fee that the outcome's amounts add up
    /// to
    pub async fn next_steps(
        &self,
        connection: &RouterConnection,
    ) -> Result<NextSteps, NextStepsError> {
        let outcome = connection
            .outcome(
                self.quote.quote_id,
                &self.tx_hashes,
                OutcomeWaitUntil::Executed,
            )
            .await?;
        let transactions = validate::check_next_steps(&self.request, &self.quote, &outcome)?;
        Ok(NextSteps {
            trader_account_id: self.request.trader_account_id.clone(),
            outcome,
            transactions,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NextStepsError {
    #[error(transparent)]
    Outcome(#[from] OutcomeError),
    #[error(transparent)]
    Unsafe(#[from] UnsafeTransactionsError),
}

/// Of a swap's outcome, checked
#[derive(Debug, Clone)]
pub struct NextSteps {
    trader_account_id: AccountId,
    outcome: SwapOutcome,
    transactions: Vec<Transaction>,
}

impl NextSteps {
    pub fn outcome(&self) -> &SwapOutcome {
        &self.outcome
    }

    pub fn transactions(&self) -> &[Transaction] {
        &self.transactions
    }

    /// Sends the next steps' transactions, one after another: the tokens move where the trader
    /// asked for them, and the fees are paid
    pub async fn execute<S: TransactionSender>(
        self,
        sender: &S,
    ) -> Result<Vec<CryptoHash>, ExecutionError<S::Error>> {
        send_in_order(sender, &self.trader_account_id, self.transactions).await
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError<E> {
    #[error("The sender signs as {signer}, not as the trader {trader_account_id}")]
    WrongSigner {
        signer: String,
        trader_account_id: AccountId,
    },
    /// The sender failed on a transaction, after the ones before it executed
    #[error("The transactions stopped after {} of them: {error}", .tx_hashes.len())]
    Interrupted {
        /// Of the transactions that executed before it
        tx_hashes: Vec<CryptoHash>,
        error: E,
    },
}

async fn send_in_order<S: TransactionSender>(
    sender: &S,
    trader_account_id: &AccountId,
    transactions: Vec<Transaction>,
) -> Result<Vec<CryptoHash>, ExecutionError<S::Error>> {
    if sender.account_id() != trader_account_id.as_str() {
        return Err(ExecutionError::WrongSigner {
            signer: sender.account_id().to_string(),
            trader_account_id: trader_account_id.clone(),
        });
    }
    let mut tx_hashes = Vec::with_capacity(transactions.len());
    for transaction in transactions {
        match sender.send_transaction(transaction).await {
            Ok(tx_hash) => tx_hashes.push(tx_hash),
            Err(error) => return Err(ExecutionError::Interrupted { tx_hashes, error }),
        }
    }
    Ok(tx_hashes)
}
