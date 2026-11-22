use near_kit::{
    SigningNear, TxSender, protocol::Action, transaction::ExecutedOptimistic,
    transaction::WaitLevel,
};

use crate::{CryptoHash, MaybeSend, Transaction, TransactionSender};

impl<W: WaitLevel> TransactionSender for SigningNear<W> {
    type Error = near_kit::Error;

    fn account_id(&self) -> &str {
        TxSender::account_id(self).as_str()
    }

    fn send_transaction(
        &self,
        transaction: Transaction,
    ) -> impl Future<Output = Result<CryptoHash, Self::Error>> + MaybeSend {
        let actions = transaction
            .actions
            .into_iter()
            .map(|call| {
                Action::function_call(
                    call.method_name,
                    call.args.into_bytes(),
                    call.gas,
                    call.deposit,
                )
            })
            .collect();
        let sender = self.wait_until::<ExecutedOptimistic>();
        async move {
            let outcome = sender.send(transaction.receiver_id, actions).await?;
            Ok(CryptoHash(*outcome.transaction_hash().as_bytes()))
        }
    }
}
