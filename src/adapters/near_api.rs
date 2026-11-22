use std::{str::FromStr, sync::Arc};

use near_api::{
    NetworkConfig, Signer,
    errors::ExecuteTransactionError,
    types::{
        Action, TxExecutionStatus,
        transaction::{actions::FunctionCallAction, result::TransactionResult},
    },
};

use crate::{CryptoHash, Transaction, TransactionSender};

/// Signs as `account_id` with `signer`, and sends to `network`
#[derive(Clone)]
pub struct NearApiSender {
    pub account_id: near_api::AccountId,
    pub signer: Arc<Signer>,
    pub network: NetworkConfig,
}

#[derive(Debug, thiserror::Error)]
pub enum NearApiSendError {
    /// near-api has another version of account IDs
    #[error("{receiver_id} isn't an account ID of near-api: {error}")]
    ReceiverId {
        receiver_id: String,
        error: <near_api::AccountId as FromStr>::Err,
    },
    #[error(transparent)]
    Execute(#[from] ExecuteTransactionError),
    #[error("The RPC answered before the transaction executed, once it was {0:?}")]
    NotExecuted(TxExecutionStatus),
}

impl TransactionSender for NearApiSender {
    type Error = NearApiSendError;

    fn account_id(&self) -> &str {
        self.account_id.as_str()
    }

    async fn send_transaction(&self, transaction: Transaction) -> Result<CryptoHash, Self::Error> {
        let receiver_id = transaction.receiver_id.as_str();
        let receiver_id = receiver_id
            .parse()
            .map_err(|error| NearApiSendError::ReceiverId {
                receiver_id: receiver_id.to_string(),
                error,
            })?;
        let actions = transaction
            .actions
            .into_iter()
            .map(|call| {
                Action::FunctionCall(Box::new(FunctionCallAction {
                    method_name: call.method_name,
                    args: call.args.into_bytes(),
                    gas: call.gas,
                    deposit: call.deposit,
                }))
            })
            .collect();
        let result = near_api::Transaction::construct(self.account_id.clone(), receiver_id)
            .add_actions(actions)
            .with_signer(self.signer.clone())
            .wait_until(TxExecutionStatus::ExecutedOptimistic)
            .send_to(&self.network)
            .await?;
        match result {
            TransactionResult::Full(result) => Ok(CryptoHash(result.outcome().transaction_hash.0)),
            TransactionResult::Pending { status } => Err(NearApiSendError::NotExecuted(status)),
        }
    }
}
