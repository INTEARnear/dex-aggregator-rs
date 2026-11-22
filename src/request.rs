use std::{collections::BTreeMap, fmt, str::FromStr};

use near_account_id::AccountId;
use serde::{Deserialize, Deserializer};

pub(crate) const WRAP_NEAR: &str = "wrap.near";

/// A token where the trader holds it, as the router names it: `near`, `nep141:<contract>` (or
/// just the contract), `rhea-nep141:<contract>` for a balance on Rhea, and `intear-dex:near` or
/// `intear-dex:nep141:<contract>` for a balance on Intear DEX
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TokenId {
    Near,
    Nep141(AccountId),
    Nep141OnRhea(AccountId),
    IntearDex(IntearAsset),
}

/// The assets that routes swap on Intear DEX
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IntearAsset {
    Near,
    Nep141(AccountId),
}

#[derive(Debug, thiserror::Error)]
#[error("{token} isn't a token the router swaps: {reason}")]
pub struct InvalidTokenId {
    token: String,
    reason: String,
}

impl TokenId {
    /// The NEP-141 it is, wNEAR for NEAR
    pub(crate) fn contract(&self) -> &str {
        match self {
            TokenId::Near | TokenId::IntearDex(IntearAsset::Near) => WRAP_NEAR,
            TokenId::Nep141(contract)
            | TokenId::Nep141OnRhea(contract)
            | TokenId::IntearDex(IntearAsset::Nep141(contract)) => contract.as_str(),
        }
    }
}

impl fmt::Display for TokenId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenId::Near => f.write_str("near"),
            TokenId::Nep141(contract) => write!(f, "nep141:{contract}"),
            TokenId::Nep141OnRhea(contract) => write!(f, "rhea-nep141:{contract}"),
            TokenId::IntearDex(IntearAsset::Near) => f.write_str("intear-dex:near"),
            TokenId::IntearDex(IntearAsset::Nep141(contract)) => {
                write!(f, "intear-dex:nep141:{contract}")
            }
        }
    }
}

impl FromStr for TokenId {
    type Err = InvalidTokenId;

    fn from_str(token: &str) -> Result<Self, Self::Err> {
        let invalid = |reason: String| InvalidTokenId {
            token: token.to_string(),
            reason,
        };
        let contract = |contract: &str| {
            contract
                .parse::<AccountId>()
                .map_err(|error| invalid(error.to_string()))
        };
        if token == "near" {
            Ok(TokenId::Near)
        } else if let Some(contract_id) = token.strip_prefix("rhea-nep141:") {
            Ok(TokenId::Nep141OnRhea(contract(contract_id)?))
        } else if token == "intear-dex:near" {
            Ok(TokenId::IntearDex(IntearAsset::Near))
        } else if let Some(contract_id) = token.strip_prefix("intear-dex:nep141:") {
            Ok(TokenId::IntearDex(IntearAsset::Nep141(contract(
                contract_id,
            )?)))
        } else if token.starts_with("intear-dex:") {
            Err(invalid("Intear DEX only has NEAR and NEP-141s".to_string()))
        } else {
            Ok(TokenId::Nep141(contract(
                token.strip_prefix("nep141:").unwrap_or(token),
            )?))
        }
    }
}

impl<'de> Deserialize<'de> for TokenId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        crate::types::parsed(deserializer)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapAmount {
    ExactIn(u128),
    ExactOut(u128),
}

/// Fractions of the amount, `0.01` is 1%
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Slippage {
    Fixed(f64),
    Auto { min: f64, max: f64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwapRequest {
    pub token_in: TokenId,
    pub token_out: TokenId,
    pub amount: SwapAmount,
    pub slippage: Slippage,
    /// Who signs the quotes' transactions
    pub trader_account_id: AccountId,
    /// Gives the quotes a nonce and block hash to sign their transactions with
    pub signing_public_key: Option<String>,
    /// All of them when not given
    pub dexes: Option<Vec<String>>,
    pub referrer_id: Option<AccountId>,
    /// Fractions of the input that each account takes, like `0.005` for 0.5%
    pub fees: BTreeMap<AccountId, f64>,
    /// Takes what a swap gives above the quote's estimate, or takes below it
    pub quote_improvement_receiver: Option<AccountId>,
}

impl SwapRequest {
    pub fn new(
        token_in: TokenId,
        token_out: TokenId,
        amount: SwapAmount,
        slippage: Slippage,
        trader_account_id: AccountId,
    ) -> Self {
        Self {
            token_in,
            token_out,
            amount,
            slippage,
            trader_account_id,
            signing_public_key: None,
            dexes: None,
            referrer_id: None,
            fees: BTreeMap::new(),
            quote_improvement_receiver: None,
        }
    }

    /// The query string of a route request
    pub(crate) fn route_query(&self) -> String {
        let mut query = form_urlencoded::Serializer::new(String::new());
        query
            .append_pair("token_in", &self.token_in.to_string())
            .append_pair("token_out", &self.token_out.to_string());
        match self.amount {
            SwapAmount::ExactIn(amount) => query.append_pair("amount_in", &amount.to_string()),
            SwapAmount::ExactOut(amount) => query.append_pair("amount_out", &amount.to_string()),
        };
        match self.slippage {
            Slippage::Fixed(slippage) => query
                .append_pair("slippage_type", "Fixed")
                .append_pair("slippage", &slippage.to_string()),
            Slippage::Auto { min, max } => query
                .append_pair("slippage_type", "Auto")
                .append_pair("min_slippage", &min.to_string())
                .append_pair("max_slippage", &max.to_string()),
        };
        query.append_pair("trader_account_id", self.trader_account_id.as_str());
        if let Some(signing_public_key) = &self.signing_public_key {
            query.append_pair("signing_public_key", signing_public_key);
        }
        if let Some(dexes) = &self.dexes {
            query.append_pair("dexes", &dexes.join(","));
        }
        if let Some(referrer_id) = &self.referrer_id {
            query.append_pair("referrer_id", referrer_id.as_str());
        }
        if !self.fees.is_empty() {
            let fees =
                serde_json::to_string(&self.fees).expect("Account IDs and numbers serialize");
            query.append_pair("fees", &fees);
        }
        if let Some(receiver_id) = &self.quote_improvement_receiver {
            query.append_pair("quote_improvement_receiver", receiver_id.as_str());
        }
        query.finish()
    }
}
