#[cfg(any(feature = "near-api", feature = "near-kit"))]
pub mod adapters;
mod connection;
mod platform;
mod request;
mod sender;
mod swap;
mod types;
mod validate;

pub use connection::{
    ConnectionClosed, ConnectionStatus, OutcomeError, QuoteSubscription, QuoteUpdate,
    ROUTER_SUBSCRIBE_URL, RejectedQuote, RouterConnection, RouterConnectionOptions, RouterError,
};
pub use platform::{MaybeSend, MaybeSync};
pub use request::{IntearAsset, InvalidTokenId, Slippage, SwapAmount, SwapRequest, TokenId};
pub use sender::{FunctionCall, Transaction, TransactionSender};
pub use swap::{CheckedQuote, ExecutedSwap, ExecutionError, NextSteps, NextStepsError};
pub use types::{
    BalanceChange, CryptoHash, ExecutionInstruction, InvalidCryptoHash, OutcomeWaitUntil, Quote,
    RouterAction, RouterAmount, RouterFunctionCall, SigningContext, SwapOutcome,
};
pub use validate::{
    MAX_ORACLE_FEES, MAX_STORAGE_DEPOSIT_FOR_OTHERS, MAX_STORAGE_DEPOSITS, UnsafeTransactionsError,
};
