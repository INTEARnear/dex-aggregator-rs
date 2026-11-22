#![allow(
    dead_code,
    reason = "Fields are typed to be checked, not all of them read"
)]

use intear_dex_common::{AssetId, TokenAmount};
use intear_dex_types::{
    AccountOrDexId, DirectWithdrawAmount, Operation, QuoteImprovementFee, ReferralFees,
    SignedFeeDiscount, SwapOperationAmount,
};
use near_account_id::AccountId;
use serde::Deserialize;

/// An amount, which contracts take as a string
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct U128(#[serde(deserialize_with = "crate::types::parsed")] pub u128);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

/// Of a NEP-141, and of Rhea
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageDeposit {
    pub registration_only: bool,
    pub account_id: Option<AccountId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NearWithdraw {
    pub amount: U128,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FtTransferCall {
    pub receiver_id: AccountId,
    pub amount: U128,
    pub msg: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FtTransfer {
    pub receiver_id: AccountId,
    pub amount: U128,
    pub memo: String,
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

/// The arguments of Intear DEX's methods. What they take is of `intear_dex_types`, as the engine
/// reads it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterAssets {
    pub asset_ids: Vec<AssetId>,
    #[serde(rename = "for")]
    pub owner: Option<AccountOrDexId>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepositNear {
    /// Without them, into the trader's own balance
    pub operations: Option<EngineOperations>,
}

/// Of the trader's balance, to the trader
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IntearDexWithdraw {
    pub asset_id: AssetId,
    pub amount: DirectWithdrawAmount,
}

/// Of `execute_operations`, `deposit_near`, and an `ft_transfer_call` to Intear DEX, whose other
/// form, a list of the operations alone, routes don't send
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineOperations {
    pub operations: Vec<Operation>,
    pub referrer: Option<AccountId>,
    pub referral_fees: Option<ReferralFees>,
    /// Only lowers the swaps' pool fees, and the engine checks its signature
    pub fee_discount: Option<SignedFeeDiscount>,
}

/// What a swap of either kind swaps
pub struct EngineSwap<'a> {
    pub asset_in: &'a AssetId,
    pub asset_out: &'a AssetId,
    pub amount: &'a SwapOperationAmount,
    pub constraint: Option<TokenAmount>,
    pub quote_improvement_fee: Option<&'a QuoteImprovementFee>,
}

pub fn engine_swap(operation: &Operation) -> Option<EngineSwap<'_>> {
    match operation {
        Operation::SwapSimple {
            asset_in,
            asset_out,
            amount,
            constraint,
            quote_improvement_fee,
            ..
        }
        | Operation::SwapAdvanced {
            asset_in,
            asset_out,
            amount,
            constraint,
            quote_improvement_fee,
            ..
        } => Some(EngineSwap {
            asset_in,
            asset_out,
            amount,
            constraint: *constraint,
            quote_improvement_fee: quote_improvement_fee.as_ref(),
        }),
        _ => None,
    }
}
