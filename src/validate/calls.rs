//! The arguments of the calls that routes make, with the fields that the checks read. The router can
//! pass others to tokens and to Intear DEX, which the checks leave to it. Rhea and Rhea DCL take
//! exactly the arguments that routes pass, as what others of theirs do isn't known to the checks.

#![allow(
    dead_code,
    reason = "Fields are typed to be checked, not all of them read"
)]

use std::fmt;

use intear_dex_common::{AssetId, TokenAmount};
use intear_dex_types::{AccountOrDexId, QuoteImprovementFee, ReferralFees, SwapOperationAmount};
use near_account_id::AccountId;
use serde::{
    Deserialize, Deserializer,
    de::{self, IgnoredAny, MapAccess, Visitor},
};

/// An amount, which contracts take as a string
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct U128(#[serde(deserialize_with = "crate::types::parsed")] pub u128);

/// Of a NEP-141
#[derive(Deserialize)]
pub struct StorageDeposit {
    pub registration_only: bool,
    pub account_id: Option<AccountId>,
}

/// Of Intear DEX, which only the account it's for matters of
#[derive(Deserialize)]
pub struct AccountStorageDeposit {
    pub account_id: Option<AccountId>,
}

#[derive(Deserialize)]
pub struct FtTransferCall {
    pub receiver_id: AccountId,
    pub amount: U128,
    pub msg: String,
}

#[derive(Deserialize)]
pub struct FtTransfer {
    pub receiver_id: AccountId,
    pub amount: U128,
    pub memo: String,
}

/// The arguments of Rhea and Rhea DCL, all of them
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RheaStorageDeposit {
    pub registration_only: bool,
    pub account_id: Option<AccountId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RheaRegisterTokens {
    pub token_ids: Vec<AccountId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RheaSwap {
    pub actions: Vec<RheaAction>,
    pub referral_id: Option<AccountId>,
    pub skip_degen_price_sync: Option<bool>,
}

/// Of an `ft_transfer_call` to Rhea
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RheaMessage {
    pub force: Option<u8>,
    pub actions: Vec<RheaAction>,
    pub skip_degen_price_sync: Option<bool>,
    pub skip_unwrap_near: Option<bool>,
    pub referral_id: Option<AccountId>,
}

/// Without `amount_in` when it swaps what the action before it gave
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RheaAction {
    pub pool_id: u64,
    pub token_in: AccountId,
    pub token_out: AccountId,
    pub amount_in: Option<U128>,
    pub amount_out: Option<U128>,
    pub min_amount_out: U128,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MftTransfer {
    pub token_id: String,
    pub receiver_id: AccountId,
    pub amount: U128,
    pub memo: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RheaWithdraw {
    pub token_id: AccountId,
    pub amount: U128,
    pub skip_unwrap_near: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RheaDclStorageDeposit {
    pub registration_only: bool,
}

/// Of an `ft_transfer_call` to Rhea DCL
#[derive(Deserialize)]
pub enum RheaDclMessage {
    Swap(RheaDclSwap),
    SwapByOutput(RheaDclSwapByOutput),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RheaDclSwap {
    pub pool_ids: Vec<String>,
    pub output_token: AccountId,
    pub min_output_amount: U128,
    pub skip_unwrap_near: Option<bool>,
    pub referral_id: Option<AccountId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RheaDclSwapByOutput {
    pub pool_ids: Vec<String>,
    pub output_token: AccountId,
    pub output_amount: U128,
    pub skip_unwrap_near: Option<bool>,
    pub referral_id: Option<AccountId>,
}

/// The arguments of Intear DEX's methods, of the types of `intear_dex_types`, as the engine reads
/// them
#[derive(Deserialize)]
pub struct RegisterAssets {
    #[serde(rename = "for")]
    pub owner: Option<AccountOrDexId>,
}

#[derive(Deserialize)]
pub struct DepositNear {
    /// Without them, into the trader's own balance
    pub operations: Option<EngineOperations>,
}

/// Of the trader's balance
#[derive(Deserialize)]
pub struct IntearDexWithdraw {
    pub asset_id: AssetId,
    /// The trader when not given
    pub withdraw_to: Option<AccountId>,
}

/// Of `execute_operations`, `deposit_near`, and an `ft_transfer_call` to Intear DEX, whose other
/// form, a list of the operations alone, routes don't send
#[derive(Deserialize)]
pub struct EngineOperations {
    pub operations: Vec<EngineOperation>,
    pub referral_fees: Option<ReferralFees>,
}

/// An operation of Intear DEX, of the ones that take or send the trader's assets
pub enum EngineOperation {
    OracleRequest(OracleRequest),
    /// `SwapSimple` or `SwapAdvanced`
    Swap(EngineSwap),
    Withdraw(EngineWithdraw),
    /// Of the trader's assets otherwise, like a transfer or a call to a dex, which routes don't
    /// make
    MovesAssets(String),
    /// Like registering assets, or an operation of a newer engine
    MovesNoAssets,
}

#[derive(Deserialize)]
pub struct OracleRequest {
    pub fee: (AssetId, TokenAmount),
    pub swap: Option<OracleFeeSwap>,
}

/// Buys the oracle's fee
#[derive(Deserialize)]
pub struct OracleFeeSwap {
    pub asset_in: AssetId,
    pub constraint: Option<TokenAmount>,
}

#[derive(Deserialize)]
pub struct EngineSwap {
    pub asset_in: AssetId,
    pub asset_out: AssetId,
    pub amount: SwapOperationAmount,
    pub constraint: Option<TokenAmount>,
    pub quote_improvement_fee: Option<QuoteImprovementFee>,
}

#[derive(Deserialize)]
pub struct EngineWithdraw {
    pub asset_id: AssetId,
    /// The trader when not given
    pub to: Option<AccountId>,
}

impl<'de> Deserialize<'de> for EngineOperation {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(EngineOperationVisitor)
    }
}

struct EngineOperationVisitor;

impl<'de> Visitor<'de> for EngineOperationVisitor {
    type Value = EngineOperation;

    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("an operation of Intear DEX")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<EngineOperation, A::Error> {
        let Some(kind) = map.next_key::<String>()? else {
            return Err(de::Error::custom("An operation has no kind"));
        };
        let operation = match kind.as_str() {
            "OracleRequest" => EngineOperation::OracleRequest(map.next_value()?),
            "SwapSimple" | "SwapAdvanced" => EngineOperation::Swap(map.next_value()?),
            "Withdraw" => EngineOperation::Withdraw(map.next_value()?),
            "DexCall" | "TransferAsset" | "StorageDeposit" => {
                map.next_value::<IgnoredAny>()?;
                EngineOperation::MovesAssets(kind)
            }
            _ => {
                map.next_value::<IgnoredAny>()?;
                EngineOperation::MovesNoAssets
            }
        };
        if map.next_key::<IgnoredAny>()?.is_some() {
            return Err(de::Error::custom("An operation has more than one kind"));
        }
        Ok(operation)
    }
}
