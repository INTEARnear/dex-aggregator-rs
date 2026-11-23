mod calls;
mod objects;
#[cfg(test)]
mod tests;

use std::collections::{BTreeSet, HashMap};

use base64::{Engine, prelude::BASE64_STANDARD};
use intear_dex_common::{AssetId, SwapRequestAmount};
use intear_dex_types::{AccountOrDexId, QuoteImprovementFee, ReferralFees, SwapOperationAmount};
use near_account_id::{AccountId, AccountIdRef};
use near_gas::NearGas;
use near_token::NearToken;
use serde::de::DeserializeOwned;

use crate::{
    request::{IntearAsset, Slippage, SwapAmount, SwapRequest, TokenId, WRAP_NEAR},
    sender::{FunctionCall, Transaction},
    types::{
        ExecutionInstruction, Quote, RouterAction, RouterAmount, RouterFunctionCall, SwapOutcome,
    },
};
use calls::*;

const RHEA: &str = "v2.ref-finance.near";
const WRAP_NEAR_ID: &AccountIdRef = AccountIdRef::new_or_panic(WRAP_NEAR);
const RHEA_DCL: &str = "dclv2.ref-labs.near";
const INTEAR_DEX: &str = "dex.intear.near";
/// Takes a share of every referral fee
const AGGREGATOR_FEE_RECEIVER: &str = "dex-aggregator.intear.near";
const FULL_FEE_FRACTION: u128 = 1_000_000;
/// The most that the router attaches to a call, with room in its transaction for the others
const MAX_GAS: NearGas = NearGas::from_tgas(900);
/// Each path of a split route rounds its own worst case
const WORST_CASE_ROUNDING: u128 = 32;
const TRADING_FEE_MEMO_PREFIX: &str = "Trading fee (";
const QUOTE_IMPROVEMENT_FEE_MEMO: &str = "Quote improvement fee";
/// As many as the router finds the outcome of
const MAX_TRANSACTIONS: usize = 20;
/// The most that the storage deposits of a quote attach in all, and again of its next steps,
/// which have no figure of them. A trader new to Rhea DCL deposits its minimum of 0.5 NEAR.
pub const MAX_STORAGE_DEPOSITS: NearToken = NearToken::from_near(1);
/// The most that Rhea's storage deposits leave to each account other than the trader, which can
/// withdraw them. A fee receiver new to Rhea takes 0.0112 NEAR.
pub const MAX_STORAGE_DEPOSIT_FOR_OTHERS: NearToken = NearToken::from_millinear(50);
/// The most that the trader pays Intear DEX's oracles of each asset of the input, out of it or by
/// a swap that buys their fee with it. Oracles aren't paid with other assets.
pub const MAX_ORACLE_FEES: &[(&str, u128)] = &[
    ("near", 1_000_000_000_000_000_000_000_000), // 1 NEAR
    ("nep141:wrap.near", 1_000_000_000_000_000_000_000_000), // 1 NEAR
    ("nep141:usdt.tether-token.near", 1_000_000), // 1 USDT
    (
        "nep141:17208628f84f5d6ad33f0da3bbbeb27ffcb398eac501a31bd6ad2011e36133a1",
        1_000_000,
    ), // 1 USDC
    (
        "nep141:token.intear.near",
        1_000_000_000_000_000_000_000_000_000,
    ), // 1000 TEAR
];

/// What makes the router's transactions unsafe to sign, one problem each
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("The router's transactions aren't safe to sign: {}", .problems.join("; "))]
pub struct UnsafeTransactionsError {
    pub problems: Vec<String>,
}

/// The quote's transactions, once they only do what the request asks: spend at most the input the
/// trader allows (the exact input, or the quote's worst case of exact-out) with the fees that the
/// next steps pay, get at least the quote's worst case of the output, call only the tokens of the
/// swap and the DEXes the router uses, attach no more to storage deposits than the quote says and
/// the limits allow, and take fees only for the requested receivers, no more than requested.
/// Fields and operations of Intear DEX and of tokens that do none of this are left to the router, so
/// that it can add them. Rhea and Rhea DCL are called with exactly the arguments that routes pass,
/// as what others of theirs do isn't known to the checks.
pub(crate) fn check_quote(
    request: &SwapRequest,
    quote: &Quote,
) -> Result<Vec<Transaction>, UnsafeTransactionsError> {
    let mut checker = TransactionChecker::new(request, quote, Phase::Quote);
    if let Err(problem) = checker.amounts() {
        checker.problem(format!("The quote's amounts: {problem}"));
    }
    checker.transaction_count(&quote.execution_instructions);
    let transactions = quote
        .execution_instructions
        .iter()
        .map(|instruction| {
            let ExecutionInstruction::NearTransaction {
                is_fee_transaction, ..
            } = instruction;
            if *is_fee_transaction {
                checker.problem("A quote whose fees are in the next steps has fee transactions");
            }
            checker.instruction(instruction)
        })
        .collect();
    if let Err(problem) = checker.quote_totals() {
        checker.problem(problem);
    }
    checker.into_transactions(transactions)
}

/// The next steps of the quote's outcome, once they only move the trader's input and output
/// between the places they hold them, and, after a swap that went through, pay its fees and the
/// quote improvement fee that the outcome's amounts add up to
pub(crate) fn check_next_steps(
    request: &SwapRequest,
    quote: &Quote,
    outcome: &SwapOutcome,
) -> Result<Vec<Transaction>, UnsafeTransactionsError> {
    let mut checker = TransactionChecker::new(request, quote, Phase::NextSteps);
    checker.transaction_count(&outcome.next_steps);
    let transactions = outcome
        .next_steps
        .iter()
        .filter_map(|instruction| {
            let ExecutionInstruction::NearTransaction {
                is_fee_transaction, ..
            } = instruction;
            if *is_fee_transaction && !outcome.charge_fee {
                checker.problem(
                    "The next steps pay fees, which are only due after a swap that went through",
                );
                return None;
            }
            Some(checker.instruction(instruction))
        })
        .collect();
    if let Err(problem) = checker.next_step_totals(outcome) {
        checker.problem(problem);
    }
    checker.into_transactions(transactions)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Quote,
    NextSteps,
}

struct TransactionChecker<'a> {
    request: &'a SwapRequest,
    quote: &'a Quote,
    phase: Phase,
    problems: Vec<String>,
    input_assets: Vec<AssetId>,
    output_assets: Vec<AssetId>,
    fee_receivers: BTreeSet<&'a str>,
    /// Of the requested fees, in millionths of the input
    requested_fee: u128,
    /// Of the input, sent to DEXes and to fee receivers
    spent: u128,
    /// Paid out of the wallet balance of the input
    wallet_fees: u128,
    /// Paid out of the balance of the input on Rhea
    rhea_fees: u128,
    /// Taken by Intear DEX at most, out of the input of its operations
    intear_dex_fees: u128,
    storage_deposits: u128,
    /// Of accounts other than the trader, which they can withdraw
    others_rhea_storage: HashMap<AccountId, u128>,
    /// Native NEAR made into wNEAR
    wrapped: u128,
    /// What the swaps give at least
    guaranteed_output: u128,
    swaps: usize,
    quote_improvement_fee: u128,
}

impl<'a> TransactionChecker<'a> {
    fn new(request: &'a SwapRequest, quote: &'a Quote, phase: Phase) -> Self {
        Self {
            request,
            quote,
            phase,
            problems: Vec::new(),
            input_assets: intear_assets(&request.token_in),
            output_assets: intear_assets(&request.token_out),
            fee_receivers: request
                .fees
                .keys()
                .map(|receiver_id| receiver_id.as_str())
                .chain([AGGREGATOR_FEE_RECEIVER])
                .collect(),
            // Added up before rounding to whole millionths, so that a fraction below 0, which the
            // router doesn't take, lowers it, and a sum that's too large is the most there is
            requested_fee: request
                .fees
                .values()
                .map(|fraction| (fraction * FULL_FEE_FRACTION as f64).round())
                .sum::<f64>() as u128,
            spent: 0,
            wallet_fees: 0,
            rhea_fees: 0,
            intear_dex_fees: 0,
            storage_deposits: 0,
            others_rhea_storage: HashMap::new(),
            wrapped: 0,
            guaranteed_output: 0,
            swaps: 0,
            quote_improvement_fee: 0,
        }
    }

    fn problem(&mut self, problem: impl Into<String>) {
        self.problems.push(problem.into());
    }

    fn into_transactions(
        self,
        transactions: Vec<Transaction>,
    ) -> Result<Vec<Transaction>, UnsafeTransactionsError> {
        if self.problems.is_empty() {
            Ok(transactions)
        } else {
            Err(UnsafeTransactionsError {
                problems: self.problems,
            })
        }
    }

    fn exact_in(&self) -> bool {
        matches!(self.request.amount, SwapAmount::ExactIn(_))
    }

    /// What the quote estimates: the output of exact-in, the input of exact-out
    fn estimated(&self) -> Result<u128, String> {
        quoted(self.quote.estimated_amount, self.exact_in())
    }

    /// The quote's worst case is within the slippage of its estimate
    fn amounts(&mut self) -> Result<(), String> {
        let estimated = self.estimated()?;
        let worst_case = quoted(self.quote.worst_case_amount, self.exact_in())?;
        let slippage = match self.request.slippage {
            Slippage::Fixed(slippage) => slippage,
            Slippage::Auto { max, .. } => max,
        };
        let max_slippage = sum(
            fraction_of(estimated, millionths(slippage))?,
            WORST_CASE_ROUNDING,
        )?;
        let beyond_slippage = if self.exact_in() {
            worst_case < estimated.saturating_sub(max_slippage)
        } else {
            worst_case > estimated.saturating_add(max_slippage)
        };
        if beyond_slippage {
            self.problem(format!(
                "The worst case {worst_case} is further from the estimate {estimated} than the slippage allows"
            ));
        }
        Ok(())
    }

    fn transaction_count(&mut self, instructions: &[ExecutionInstruction]) {
        if instructions.len() > MAX_TRANSACTIONS {
            self.problem(format!(
                "Has {} transactions, more than {MAX_TRANSACTIONS}",
                instructions.len()
            ));
        }
    }

    fn instruction(&mut self, instruction: &ExecutionInstruction) -> Transaction {
        let ExecutionInstruction::NearTransaction {
            receiver_id,
            actions,
            is_fee_transaction,
        } = instruction;
        let actions = actions
            .iter()
            .filter_map(|RouterAction::FunctionCall(call)| {
                self.function_call(receiver_id, call, *is_fee_transaction)
                    .map_err(|problem| {
                        self.problem(format!("{receiver_id} {}: {problem}", call.method_name));
                    })
                    .ok()
            })
            .collect();
        Transaction {
            receiver_id: receiver_id.clone(),
            actions,
        }
    }

    fn quote_totals(&mut self) -> Result<(), String> {
        // What the trader allows the swap to take
        let max_input = match self.request.amount {
            SwapAmount::ExactIn(amount) => amount,
            SwapAmount::ExactOut(_) => quoted(self.quote.worst_case_amount, false)?,
        };
        let fee_amount = self.quote.fee_amount;
        // The fee transactions of the next steps pay the fee amount after these
        let spent = sum(self.spent, fee_amount)?;
        if spent > max_input {
            self.problem(format!(
                "The transactions spend {spent} of the input, more than the {max_input} the trader allows"
            ));
        }
        let storage_deposits = self.quote.storage_deposits.as_yoctonear();
        if self.storage_deposits > storage_deposits {
            self.problem(format!(
                "The transactions attach {} to storage deposits, more than the quote's {storage_deposits}",
                self.storage_deposits
            ));
        }
        self.storage_deposits_within_limit();
        if self.wrapped > 0
            && !(self.request.token_in.contract() == WRAP_NEAR && self.wrapped <= max_input)
        {
            self.problem(format!(
                "The transactions wrap {} NEAR, which isn't the input",
                self.wrapped
            ));
        }
        let max_fees = sum(
            fraction_of(max_input, self.requested_fee)?,
            self.fee_receivers.len() as u128,
        )?;
        let fees = sum(sum(fee_amount, self.rhea_fees)?, self.intear_dex_fees)?;
        if fees > max_fees {
            self.problem(format!(
                "The fees of {fees} are more than the requested fees of at most {max_fees}"
            ));
        }
        if self.swaps == 0 && self.request.token_in.contract() != self.request.token_out.contract()
        {
            self.problem("The transactions don't swap");
        }
        if self.swaps > 0 {
            let (least_output, least_output_name) = match self.request.amount {
                SwapAmount::ExactIn(_) => (
                    quoted(self.quote.worst_case_amount, true)?,
                    "the quote's worst case",
                ),
                SwapAmount::ExactOut(amount) => (amount, "the requested output"),
            };
            if self.guaranteed_output < least_output {
                self.problem(format!(
                    "The swaps guarantee {} of the output, less than {least_output_name} of {least_output}",
                    self.guaranteed_output
                ));
            }
        }
        Ok(())
    }

    fn next_step_totals(&mut self, outcome: &SwapOutcome) -> Result<(), String> {
        self.storage_deposits_within_limit();
        let fee_amount = self.quote.fee_amount;
        if self.wallet_fees > fee_amount {
            self.problem(format!(
                "The next steps pay {} of fees, more than the quote's {fee_amount}",
                self.wallet_fees
            ));
        }
        let quote_improvement_fee = outcome.quote_improvement_fee.unwrap_or(0);
        if self.quote_improvement_fee != quote_improvement_fee {
            self.problem(format!(
                "The next steps pay a quote improvement fee of {}, not the outcome's {quote_improvement_fee}",
                self.quote_improvement_fee
            ));
        }
        // Takes what beats the estimate, so that the trader gets it of exact-in, and spends it of
        // exact-out, with the fees
        if quote_improvement_fee > 0 {
            let estimated = self.estimated()?;
            let (amount, gets_or_spends) = if self.exact_in() {
                (outcome.amount_out, "gets")
            } else {
                (outcome.amount_in, "spends")
            };
            if amount != estimated {
                self.problem(format!(
                    "With a quote improvement fee of {quote_improvement_fee}, the trader {gets_or_spends} {amount}, not the quote's estimate of {estimated}"
                ));
            }
        }
        Ok(())
    }

    fn function_call(
        &mut self,
        receiver_id: &AccountId,
        call: &RouterFunctionCall,
        is_fee_transaction: bool,
    ) -> Result<FunctionCall, String> {
        if call.gas > MAX_GAS {
            return Err(format!(
                "Attaches {} gas, more than the router attaches to a call",
                call.gas.as_gas()
            ));
        }
        let args = BASE64_STANDARD
            .decode(&call.args)
            .map_err(|error| format!("Its arguments aren't base64: {error}"))?;
        let args = String::from_utf8(args)
            .map_err(|error| format!("Its arguments aren't JSON: {error}"))?;
        let method = call.method_name.as_str();
        let attached = call.deposit.as_yoctonear();
        match receiver_id.as_str() {
            RHEA => self.rhea(method, &args, attached)?,
            RHEA_DCL => self.rhea_dcl(method, &args, attached)?,
            INTEAR_DEX => self.intear_dex(method, &args, attached)?,
            contract
                if contract == self.request.token_in.contract()
                    || contract == self.request.token_out.contract() =>
            {
                self.token(contract, method, &args, attached, is_fee_transaction)?
            }
            _ => {
                return Err(
                    "Isn't one of the swap's tokens or the DEXes the router swaps on".into(),
                );
            }
        }
        Ok(FunctionCall {
            method_name: call.method_name.clone(),
            args,
            gas: call.gas,
            deposit: call.deposit,
        })
    }

    fn token(
        &mut self,
        contract: &str,
        method: &str,
        args: &str,
        attached: u128,
        is_fee_transaction: bool,
    ) -> Result<(), String> {
        match method {
            "storage_deposit" => {
                let StorageDeposit {
                    registration_only,
                    account_id,
                } = parse(args)?;
                if !registration_only {
                    return Err("Deposits more than registration needs".into());
                }
                if let Some(account_id) = account_id {
                    self.storage_account(&account_id)?;
                }
                add(&mut self.storage_deposits, attached)
            }
            "near_deposit" => {
                wrap_near_only(contract)?;
                add(&mut self.wrapped, attached)
            }
            "near_withdraw" => {
                wrap_near_only(contract)?;
                one_yocto(attached)
            }
            "ft_transfer_call" => {
                let FtTransferCall {
                    receiver_id,
                    amount,
                    msg,
                } = parse(args)?;
                one_yocto(attached)?;
                if msg.is_empty() {
                    return match receiver_id.as_str() {
                        RHEA | INTEAR_DEX => Ok(()),
                        _ => Err(format!(
                            "Deposits into {receiver_id}, which only Rhea and Intear DEX hold for the trader"
                        )),
                    };
                }
                self.swap_call()?;
                if contract != self.request.token_in.contract() {
                    return Err("Swaps something else than the input".into());
                }
                add(&mut self.spent, amount.0)?;
                match receiver_id.as_str() {
                    RHEA => {
                        let RheaMessage { actions, .. } = parse_message(&msg)?;
                        self.rhea_actions(&actions).map(|_swapped| ())
                    }
                    RHEA_DCL => self.rhea_dcl_swap(parse_message(&msg)?),
                    INTEAR_DEX => self
                        .engine_operations(&parse_message(&msg)?)
                        .map(|_taken| ()),
                    _ => Err(format!(
                        "Sends the input to {receiver_id}, which isn't a DEX the router swaps on"
                    )),
                }
            }
            "ft_transfer" => {
                let FtTransfer {
                    receiver_id,
                    amount,
                    memo,
                } = parse(args)?;
                one_yocto(attached)?;
                if !is_fee_transaction {
                    return Err(format!(
                        "Transfers tokens to {receiver_id} outside of a fee transaction"
                    ));
                }
                if memo.starts_with(TRADING_FEE_MEMO_PREFIX) {
                    if contract != self.request.token_in.contract() {
                        return Err("Pays a referral fee in something else than the input".into());
                    }
                    self.fee_receiver(&receiver_id)?;
                    add(&mut self.wallet_fees, amount.0)?;
                    return add(&mut self.spent, amount.0);
                }
                if memo == QUOTE_IMPROVEMENT_FEE_MEMO && self.phase == Phase::NextSteps {
                    if Some(&receiver_id) != self.request.quote_improvement_receiver.as_ref() {
                        return Err(format!(
                            "Pays the quote improvement fee to {receiver_id}, not the requested receiver"
                        ));
                    }
                    let improved_token = if self.exact_in() {
                        &self.request.token_out
                    } else {
                        &self.request.token_in
                    };
                    if contract != improved_token.contract() {
                        return Err(
                            "Pays the quote improvement fee in another token than the one the swap beat the quote in".into(),
                        );
                    }
                    return add(&mut self.quote_improvement_fee, amount.0);
                }
                Err(format!(
                    "Transfers tokens to {receiver_id} with the memo {memo:?}"
                ))
            }
            _ => Err("Isn't a call that routes make to tokens".into()),
        }
    }

    fn rhea(&mut self, method: &str, args: &str, attached: u128) -> Result<(), String> {
        match method {
            "register_tokens" => {
                let RheaRegisterTokens { .. } = parse(args)?;
                one_yocto(attached)
            }
            "storage_deposit" => {
                let RheaStorageDeposit {
                    registration_only,
                    account_id,
                } = parse(args)?;
                if registration_only {
                    return Err("Rhea's storage is deposited whole".into());
                }
                add(&mut self.storage_deposits, attached)?;
                let Some(account_id) = account_id else {
                    return Ok(());
                };
                self.storage_account(&account_id)?;
                if account_id == self.request.trader_account_id {
                    return Ok(());
                }
                let left_to_account = self
                    .others_rhea_storage
                    .entry(account_id.clone())
                    .or_default();
                add(left_to_account, attached)?;
                if *left_to_account > MAX_STORAGE_DEPOSIT_FOR_OTHERS.as_yoctonear() {
                    return Err(format!(
                        "Leaves {left_to_account} of storage on Rhea to {account_id} to withdraw, more than {}",
                        MAX_STORAGE_DEPOSIT_FOR_OTHERS.as_yoctonear()
                    ));
                }
                Ok(())
            }
            "swap" => {
                let RheaSwap { actions, .. } = parse(args)?;
                one_yocto(attached)?;
                self.swap_call()?;
                if !matches!(self.request.token_in, TokenId::Nep141OnRhea(_)) {
                    return Err(
                        "Swaps the trader's balance on Rhea, which isn't where the input is".into(),
                    );
                }
                let swapped = self.rhea_actions(&actions)?;
                add(&mut self.spent, swapped)
            }
            "mft_transfer" => {
                let MftTransfer {
                    token_id,
                    receiver_id,
                    amount,
                    memo,
                } = parse(args)?;
                one_yocto(attached)?;
                if self.phase != Phase::Quote {
                    return Err("Next steps pay no fees out of the balance on Rhea".into());
                }
                if !memo.starts_with(TRADING_FEE_MEMO_PREFIX) {
                    return Err(
                        "Transfers a Rhea balance for something else than a referral fee".into(),
                    );
                }
                if token_id != self.request.token_in.contract() {
                    return Err("Pays a referral fee in something else than the input".into());
                }
                self.fee_receiver(&receiver_id)?;
                add(&mut self.rhea_fees, amount.0)?;
                add(&mut self.spent, amount.0)
            }
            "withdraw" => {
                let RheaWithdraw { token_id, .. } = parse(args)?;
                one_yocto(attached)?;
                self.own_token(token_id.as_str())
            }
            _ => Err("Isn't a call that routes make to Rhea".into()),
        }
    }

    /// The input that the actions swap, adding up the least output they give
    fn rhea_actions(&mut self, actions: &[RheaAction]) -> Result<u128, String> {
        // Rhea's actions only guarantee an output of exact-in, which routes only swap there
        if !self.exact_in() {
            return Err("Swaps exact-out on Rhea, which routes don't".into());
        }
        if actions.is_empty() {
            return Err("Has no swap actions".into());
        }
        let mut swapped = 0;
        for (index, action) in actions.iter().enumerate() {
            match action.amount_in {
                Some(amount_in) => {
                    if action.token_in != self.request.token_in.contract() {
                        return Err(format!(
                            "A path swaps {}, which isn't the input",
                            action.token_in
                        ));
                    }
                    add(&mut swapped, amount_in.0)?;
                }
                None => {
                    let previous_token_out = index
                        .checked_sub(1)
                        .map(|previous| &actions[previous].token_out);
                    if previous_token_out != Some(&action.token_in) {
                        return Err("An action doesn't swap what the one before it gave".into());
                    }
                }
            }
            let ends_path = actions
                .get(index + 1)
                .is_none_or(|next| next.amount_in.is_some());
            if ends_path {
                if action.token_out != self.request.token_out.contract() {
                    return Err(format!(
                        "A path ends in {}, which isn't the output",
                        action.token_out
                    ));
                }
                add(&mut self.guaranteed_output, action.min_amount_out.0)?;
            }
        }
        Ok(swapped)
    }

    fn rhea_dcl(&mut self, method: &str, args: &str, attached: u128) -> Result<(), String> {
        if method != "storage_deposit" {
            return Err("Isn't a call that routes make to Rhea DCL".into());
        }
        let RheaDclStorageDeposit { .. } = parse(args)?;
        add(&mut self.storage_deposits, attached)
    }

    fn rhea_dcl_swap(&mut self, message: RheaDclMessage) -> Result<(), String> {
        let (output_token, guaranteed_output) = match (message, self.request.amount) {
            (RheaDclMessage::Swap(swap), SwapAmount::ExactIn(_)) => {
                (swap.output_token, swap.min_output_amount.0)
            }
            (RheaDclMessage::SwapByOutput(swap), SwapAmount::ExactOut(requested)) => {
                if swap.output_amount.0 != requested {
                    return Err("Swaps for another amount than the requested output".into());
                }
                (swap.output_token, requested)
            }
            (RheaDclMessage::Swap(_), _) => {
                return Err("Swap isn't the swap the request asks for".into());
            }
            (RheaDclMessage::SwapByOutput(_), _) => {
                return Err("SwapByOutput isn't the swap the request asks for".into());
            }
        };
        if output_token != self.request.token_out.contract() {
            return Err(format!("Swaps to {output_token}, which isn't the output"));
        }
        add(&mut self.guaranteed_output, guaranteed_output)
    }

    fn intear_dex(&mut self, method: &str, args: &str, attached: u128) -> Result<(), String> {
        match method {
            "storage_deposit" => {
                let AccountStorageDeposit { account_id } = parse(args)?;
                if let Some(account_id) = account_id {
                    self.storage_account(&account_id)?;
                }
                add(&mut self.storage_deposits, attached)
            }
            "register_assets" => {
                let RegisterAssets { owner, .. } = parse(args)?;
                one_yocto(attached)?;
                match owner {
                    Some(AccountOrDexId::Account(account_id)) => self.storage_account(&account_id),
                    Some(AccountOrDexId::Dex(dex_id)) => Err(format!(
                        "Registers assets for the dex {dex_id}, which routes don't"
                    )),
                    None => Ok(()),
                }
            }
            "deposit_near" => {
                let DepositNear { operations } = parse(args)?;
                let Some(operations) = operations else {
                    // Into the trader's own balance
                    return Ok(());
                };
                self.swap_call()?;
                if self.request.token_in.contract() != WRAP_NEAR {
                    return Err("Deposits NEAR, which isn't the input".into());
                }
                add(&mut self.spent, attached)?;
                self.engine_operations(&operations).map(|_taken| ())
            }
            "execute_operations" => {
                let operations = parse(args)?;
                one_yocto(attached)?;
                self.swap_call()?;
                if !matches!(self.request.token_in, TokenId::IntearDex(_)) {
                    return Err(
                        "Swaps the trader's balance on Intear DEX, which isn't where the input is"
                            .into(),
                    );
                }
                let taken = self.engine_operations(&operations)?;
                add(&mut self.spent, taken)
            }
            "withdraw" => {
                let IntearDexWithdraw {
                    asset_id,
                    withdraw_to,
                } = parse(args)?;
                one_yocto(attached)?;
                self.withdrawn_to_trader(withdraw_to.as_ref())?;
                self.own_asset(&asset_id)
            }
            _ => Err("Isn't a call that routes make to Intear DEX".into()),
        }
    }

    /// What the operations take out of the trader's balance on Intear DEX
    fn engine_operations(&mut self, args: &EngineOperations) -> Result<u128, String> {
        let referral_fee = self.engine_referral_fees(args.referral_fees.as_ref())?;
        let operations: Vec<&EngineOperation> = args
            .operations
            .iter()
            .filter(|operation| !matches!(operation, EngineOperation::MovesNoAssets))
            .collect();
        let mut taken = 0;
        let mut previous_swap = None;
        for (index, operation) in operations.iter().enumerate() {
            let feeds_next_swap = matches!(
                operations.get(index + 1),
                Some(EngineOperation::Swap(next))
                    if matches!(next.amount, SwapOperationAmount::OutputOfLastIn)
            );
            match operation {
                EngineOperation::OracleRequest(OracleRequest {
                    fee: (fee_asset, fee),
                    swap: None,
                }) => {
                    if !self.input_assets.contains(fee_asset) {
                        return Err("Pays the oracle in something else than the input".into());
                    }
                    oracle_fee_within_limit(fee_asset, fee.get())?;
                    add(&mut taken, fee.get())?;
                }
                // Buys the oracle's fee with at most `constraint` of the input
                EngineOperation::OracleRequest(OracleRequest {
                    swap: Some(fee_swap),
                    ..
                }) => {
                    if !self.input_assets.contains(&fee_swap.asset_in) {
                        return Err("Buys the oracle fee with something else than the input".into());
                    }
                    let Some(constraint) = fee_swap.constraint else {
                        return Err("Buys the oracle fee with no limit of the input".into());
                    };
                    oracle_fee_within_limit(&fee_swap.asset_in, constraint.get())?;
                    add(&mut taken, constraint.get())?;
                }
                EngineOperation::Swap(swap) => {
                    let swap_taken = self.engine_swap(swap, previous_swap, feeds_next_swap)?;
                    add(&mut taken, swap_taken)?;
                }
                EngineOperation::Withdraw(EngineWithdraw { asset_id, to }) => {
                    self.own_asset(asset_id)?;
                    self.withdrawn_to_trader(to.as_ref())?;
                }
                EngineOperation::MovesAssets(kind) => {
                    return Err(format!(
                        "Moves the trader's assets with {kind}, which routes don't"
                    ));
                }
                EngineOperation::MovesNoAssets => unreachable!("Left out above"),
            }
            previous_swap = match operation {
                EngineOperation::Swap(swap) => Some(swap),
                _ => None,
            };
        }
        // Out of an exact-in swap's input, or on top of an exact-out one's within its constraint
        add(&mut self.intear_dex_fees, fraction_of(taken, referral_fee)?)?;
        Ok(taken)
    }

    /// What the swap takes at most of the input. Exact-in swaps can be chained: each swap of
    /// `OutputOfLastIn` swaps what the one before it gave, and only the last one is constrained.
    fn engine_swap(
        &mut self,
        swap: &EngineSwap,
        previous_swap: Option<&EngineSwap>,
        feeds_next_swap: bool,
    ) -> Result<u128, String> {
        let mut taken = 0;
        match &swap.amount {
            SwapOperationAmount::OutputOfLastIn => {
                if !self.exact_in() {
                    return Err(
                        "Swaps what the swap before it gave, which only exact-in swaps do".into(),
                    );
                }
                if previous_swap.is_none_or(|previous| swap.asset_in != previous.asset_out) {
                    return Err("Swaps something else than what the swap before it gave".into());
                }
            }
            SwapOperationAmount::EntireBalanceIn => {
                return Err("Swaps the entire balance, which routes don't".into());
            }
            SwapOperationAmount::Amount(amount) => {
                if !self.input_assets.contains(&swap.asset_in) {
                    return Err("Swaps something else than the input".into());
                }
                match (amount, self.request.amount) {
                    (SwapRequestAmount::ExactIn(amount), SwapAmount::ExactIn(_)) => {
                        taken = amount.get();
                    }
                    (SwapRequestAmount::ExactOut(amount), SwapAmount::ExactOut(requested)) => {
                        if amount.get() != requested {
                            return Err("Swaps for another amount than the requested output".into());
                        }
                    }
                    (SwapRequestAmount::ExactIn(_), _) => {
                        return Err("ExactIn isn't the swap the request asks for".into());
                    }
                    (SwapRequestAmount::ExactOut(_), _) => {
                        return Err("ExactOut isn't the swap the request asks for".into());
                    }
                }
            }
        }
        if feeds_next_swap {
            if swap.constraint.is_some() || swap.quote_improvement_fee.is_some() {
                return Err(
                    "Constrains a swap before the last one of a chain, which routes don't".into(),
                );
            }
            return Ok(taken);
        }
        if !self.output_assets.contains(&swap.asset_out) {
            return Err("Swaps to something else than the output".into());
        }
        let constraint = swap
            .constraint
            .ok_or("The last swap of a chain isn't constrained")?
            .get();
        self.engine_quote_improvement_fee(swap.quote_improvement_fee.as_ref())?;
        match self.request.amount {
            // Gives exactly the requested output, taking at most the constraint
            SwapAmount::ExactOut(requested) => {
                add(&mut self.guaranteed_output, requested)?;
                Ok(constraint)
            }
            SwapAmount::ExactIn(_) => {
                add(&mut self.guaranteed_output, constraint)?;
                Ok(taken)
            }
        }
    }

    fn engine_quote_improvement_fee(
        &self,
        fee: Option<&QuoteImprovementFee>,
    ) -> Result<(), String> {
        let Some(fee) = fee else {
            return Ok(());
        };
        if Some(&fee.quote_improvement_receiver) != self.request.quote_improvement_receiver.as_ref()
        {
            return Err(
                "Pays the quote improvement fee to someone else than the requested receiver".into(),
            );
        }
        let expected = fee.quote_expected_amount.get();
        let estimated = self.estimated()?;
        let beats_estimate = if self.exact_in() {
            expected < estimated
        } else {
            expected > estimated
        };
        if beats_estimate {
            return Err(format!(
                "Takes a quote improvement fee of what beats {expected}, which the quote's estimate of {estimated} doesn't"
            ));
        }
        Ok(())
    }

    /// In millionths of the input
    fn engine_referral_fees(&self, fees: Option<&ReferralFees>) -> Result<u128, String> {
        let mut total = 0;
        for (receiver_id, fraction) in fees.map(|fees| fees.0.as_slice()).unwrap_or_default() {
            self.fee_receiver(receiver_id)?;
            total += u128::from(fraction.get());
        }
        if total > self.requested_fee {
            return Err(format!(
                "Referral fees of {total} millionths are more than the requested {}",
                self.requested_fee
            ));
        }
        Ok(total)
    }

    fn swap_call(&mut self) -> Result<(), String> {
        if self.phase == Phase::NextSteps {
            return Err("Next steps don't swap".into());
        }
        self.swaps += 1;
        Ok(())
    }

    fn storage_deposits_within_limit(&mut self) {
        if self.storage_deposits > MAX_STORAGE_DEPOSITS.as_yoctonear() {
            self.problem(format!(
                "The transactions attach {} to storage deposits, more than {} in all",
                self.storage_deposits,
                MAX_STORAGE_DEPOSITS.as_yoctonear()
            ));
        }
    }

    fn storage_account(&self, account_id: &AccountId) -> Result<(), String> {
        let account_id = account_id.as_str();
        let allowed = account_id == self.request.trader_account_id.as_str()
            || account_id == RHEA
            || account_id == INTEAR_DEX
            || self.fee_receivers.contains(account_id)
            || self
                .request
                .quote_improvement_receiver
                .as_ref()
                .is_some_and(|receiver_id| receiver_id.as_str() == account_id)
            // Takes what Intear DEX can't send to the trader
            || self
                .request
                .referrer_id
                .as_ref()
                .is_some_and(|referrer_id| referrer_id.as_str() == account_id);
        if !allowed {
            return Err(format!(
                "Registers {account_id}, which the swap doesn't pay"
            ));
        }
        Ok(())
    }

    /// Of Intear DEX's withdrawals: the trader's own, when they name an account
    fn withdrawn_to_trader(&self, account_id: Option<&AccountId>) -> Result<(), String> {
        if account_id.is_some_and(|account_id| *account_id != self.request.trader_account_id) {
            return Err("Withdraws to someone else than the trader".into());
        }
        Ok(())
    }

    fn fee_receiver(&self, account_id: &AccountId) -> Result<(), String> {
        if !self.fee_receivers.contains(account_id.as_str()) {
            return Err(format!(
                "Pays a fee to {account_id}, which the request doesn't"
            ));
        }
        Ok(())
    }

    fn own_token(&self, contract: &str) -> Result<(), String> {
        if contract != self.request.token_in.contract()
            && contract != self.request.token_out.contract()
        {
            return Err(format!(
                "Moves {contract}, which isn't the input or the output"
            ));
        }
        Ok(())
    }

    fn own_asset(&self, asset_id: &AssetId) -> Result<(), String> {
        if !self.input_assets.contains(asset_id) && !self.output_assets.contains(asset_id) {
            return Err(format!(
                "Moves {asset_id}, which isn't the input or the output"
            ));
        }
        Ok(())
    }
}

/// The amount of the kind that the router quotes for the request
fn quoted(amount: RouterAmount, exact_in: bool) -> Result<u128, String> {
    match (amount, exact_in) {
        (RouterAmount::AmountOut(amount), true) | (RouterAmount::AmountIn(amount), false) => {
            Ok(amount)
        }
        (RouterAmount::AmountIn(_), true) => Err("Quotes an input of an exact-in request".into()),
        (RouterAmount::AmountOut(_), false) => {
            Err("Quotes an output of an exact-out request".into())
        }
    }
}

fn parse<T: DeserializeOwned>(args: &str) -> Result<T, String> {
    from_objects(args)
        .map_err(|error| format!("Its arguments aren't the ones routes pass: {error}"))
}

/// Of the JSON string that an `ft_transfer_call` passes to the DEX
fn parse_message<T: DeserializeOwned>(msg: &str) -> Result<T, String> {
    from_objects(msg).map_err(|error| format!("Its message isn't the one routes pass: {error}"))
}

/// Like `serde_json::from_str`, but without structs read from arrays by position
fn from_objects<T: DeserializeOwned>(json: &str) -> serde_json::Result<T> {
    let mut deserializer = serde_json::Deserializer::from_str(json);
    let value = T::deserialize(objects::ObjectsOnly(&mut deserializer))?;
    deserializer.end()?;
    Ok(value)
}

/// Of a fraction like `0.005`, whose millionths are whole in requests the router takes
fn millionths(fraction: f64) -> u128 {
    // Saturates, so that a fraction below 0 is none
    (fraction * FULL_FEE_FRACTION as f64).round() as u128
}

/// `amount * millionths / FULL_FEE_FRACTION`, rounded down, without overflowing on the way
fn fraction_of(amount: u128, millionths: u128) -> Result<u128, String> {
    let whole = (amount / FULL_FEE_FRACTION).checked_mul(millionths);
    let rest = (amount % FULL_FEE_FRACTION)
        .checked_mul(millionths)
        .map(|rest| rest / FULL_FEE_FRACTION);
    whole
        .zip(rest)
        .and_then(|(whole, rest)| whole.checked_add(rest))
        .ok_or_else(|| format!("{millionths} millionths of {amount} are too large"))
}

fn sum(amount: u128, other: u128) -> Result<u128, String> {
    amount
        .checked_add(other)
        .ok_or_else(|| format!("{amount} and {other} add up to too much"))
}

fn add(total: &mut u128, amount: u128) -> Result<(), String> {
    *total = sum(*total, amount)?;
    Ok(())
}

fn one_yocto(attached: u128) -> Result<(), String> {
    if attached > 1 {
        return Err(format!(
            "Attaches {attached}, where routes attach at most 1 yoctoNEAR"
        ));
    }
    Ok(())
}

fn oracle_fee_within_limit(asset: &AssetId, amount: u128) -> Result<(), String> {
    let asset = asset.to_string();
    let Some((_, limit)) = MAX_ORACLE_FEES
        .iter()
        .find(|(limited, _)| *limited == asset)
    else {
        return Err(format!(
            "Pays an oracle with {asset}, which oracles aren't paid with"
        ));
    };
    if amount > *limit {
        return Err(format!(
            "Pays an oracle {amount} of {asset}, more than {limit}"
        ));
    }
    Ok(())
}

/// The assets Intear DEX has the token as: NEAR is both native NEAR and wNEAR
fn intear_assets(token: &TokenId) -> Vec<AssetId> {
    match token {
        TokenId::Nep141(contract)
        | TokenId::Nep141OnRhea(contract)
        | TokenId::IntearDex(IntearAsset::Nep141(contract))
            if contract.as_str() != WRAP_NEAR =>
        {
            vec![AssetId::Nep141(contract.clone())]
        }
        _ => vec![AssetId::Near, AssetId::Nep141(WRAP_NEAR_ID.to_owned())],
    }
}

fn wrap_near_only(contract: &str) -> Result<(), String> {
    if contract != WRAP_NEAR {
        return Err("Only wNEAR wraps NEAR".into());
    }
    Ok(())
}
