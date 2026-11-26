use std::{fmt, str::FromStr};

use near_account_id::AccountId;
use near_gas::NearGas;
use near_token::NearToken;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

use crate::request::TokenId;

/// What the router quotes: the output of an exact-in request, the input of an exact-out one
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouterAmount {
    AmountIn(#[serde(deserialize_with = "parsed")] u128),
    AmountOut(#[serde(deserialize_with = "parsed")] u128),
}

/// A route of the router, of a subscription whose fees are paid in the next steps of its outcome
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Quote {
    pub dex_id: String,
    pub estimated_amount: RouterAmount,
    /// Within the requested slippage of the estimate
    pub worst_case_amount: RouterAmount,
    /// Without the fee transactions, which the outcome has in its next steps
    pub execution_instructions: Vec<ExecutionInstruction>,
    /// Where the swaps leave the output, which the next steps move to `token_out`
    pub token_output: TokenId,
    /// What the fee discount signed into the route saves, which the estimate already has
    pub discount: Option<RouterAmount>,
    pub estimated_max_gas_cost: NearToken,
    pub estimated_gas_usage: NearToken,
    pub storage_deposits: NearToken,
    /// What the next steps pay out of the wallet balance of the input token, once the swap went
    /// through
    #[serde(deserialize_with = "parsed")]
    pub fee_amount: u128,
    /// What the route's pools, and the oracle its swap asks, take of its input as fees: 0.003 is
    /// 0.3%. The referral fees aren't in it.
    #[serde(deserialize_with = "parsed")]
    pub pool_fee: f64,
    /// Of a request with `signing_public_key`, once the router found the key
    pub signing: Option<SigningContext>,
    /// What the outcome of its transactions is asked for by
    pub quote_id: i64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub enum ExecutionInstruction {
    NearTransaction {
        receiver_id: AccountId,
        actions: Vec<RouterAction>,
        /// Pays the fees, only in next steps, after a swap that went through
        #[serde(default)]
        is_fee_transaction: bool,
    },
}

/// The only action routes make
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub enum RouterAction {
    FunctionCall(RouterFunctionCall),
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouterFunctionCall {
    pub method_name: String,
    /// Base64 of JSON
    pub args: String,
    pub gas: NearGas,
    pub deposit: NearToken,
}

/// The nonce and block hash to sign a quote's transactions with, without asking an RPC
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SigningContext {
    /// Of the key when the routes were found: the first transaction takes the one after
    pub nonce: u64,
    pub block_hash: CryptoHash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeWaitUntil {
    /// Once every transaction and receipt executed, with the balances compared and the next steps
    Executed,
    /// As soon as the swaps executed, from what the DEXes logged of them, without next steps
    Swap,
}

/// What the transactions of a quote did
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SwapOutcome {
    /// Whether the swap went through, so that its fees are due
    pub charge_fee: bool,
    /// What the trader spent of the input, with the fees once the swap went through
    #[serde(deserialize_with = "parsed")]
    pub amount_in: u128,
    /// What the trader got of the output, without the quote improvement fee
    #[serde(deserialize_with = "parsed")]
    pub amount_out: u128,
    pub balance_changes: Vec<BalanceChange>,
    /// Move the tokens where the trader asked for them, and pay the fees once the swap went
    /// through
    pub next_steps: Vec<ExecutionInstruction>,
    /// Why the swap didn't go through, to show to the trader
    pub error: Option<String>,
    /// False of an outcome at the swap, whose next steps are empty
    pub settled: bool,
    /// What the next steps pay the quote improvement receiver for the swap beating the estimate
    #[serde(default, deserialize_with = "parsed_option")]
    pub quote_improvement_fee: Option<u128>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BalanceChange {
    pub token: TokenId,
    #[serde(deserialize_with = "parsed")]
    pub change: i128,
}

/// A transaction hash
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct CryptoHash(pub [u8; 32]);

#[derive(Debug, thiserror::Error)]
#[error("{0} isn't the base58 of 32 bytes")]
pub struct InvalidCryptoHash(String);

impl fmt::Display for CryptoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&bs58::encode(self.0).into_string())
    }
}

impl fmt::Debug for CryptoHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for CryptoHash {
    type Err = InvalidCryptoHash;

    fn from_str(hash: &str) -> Result<Self, Self::Err> {
        bs58::decode(hash)
            .into_vec()
            .ok()
            .and_then(|bytes| bytes.try_into().ok())
            .map(CryptoHash)
            .ok_or_else(|| InvalidCryptoHash(hash.to_string()))
    }
}

impl Serialize for CryptoHash {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for CryptoHash {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        parsed(deserializer)
    }
}

/// Of a value the router sends as a string, like amounts, which are parsed as the contracts it
/// calls parse them
pub(crate) fn parsed<'de, D: Deserializer<'de>, T: FromStr<Err: fmt::Display>>(
    deserializer: D,
) -> Result<T, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(D::Error::custom)
}

fn parsed_option<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u128>, D::Error> {
    Option::<String>::deserialize(deserializer)?
        .map(|amount| amount.parse().map_err(D::Error::custom))
        .transpose()
}
