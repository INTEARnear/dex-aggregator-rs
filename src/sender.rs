use near_account_id::AccountId;
use near_gas::NearGas;
use near_token::NearToken;

use crate::{
    platform::{MaybeSend, MaybeSync},
    types::CryptoHash,
};

/// A transaction of the router that the checks passed
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Transaction {
    pub receiver_id: AccountId,
    pub actions: Vec<FunctionCall>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionCall {
    pub method_name: String,
    /// JSON, exactly as the checks read it
    pub args: String,
    pub gas: NearGas,
    pub deposit: NearToken,
}

/// Signs transactions without asking anyone, and sends them
pub trait TransactionSender: MaybeSync {
    type Error: std::error::Error + MaybeSend + MaybeSync + 'static;

    /// Who signs, which must be the trader the quotes are for
    fn account_id(&self) -> &str;

    /// Resolves with its hash once it executed, also when a receipt of it failed, as the router
    /// tells what the transactions did, and the next steps return what a failed swap left behind
    fn send_transaction(
        &self,
        transaction: Transaction,
    ) -> impl Future<Output = Result<CryptoHash, Self::Error>> + MaybeSend;
}
