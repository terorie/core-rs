use std::{cmp::Ordering, io};

use beserial::{Deserialize, Serialize};
use hash::{Hash, HashOutput, SerializeContent};
use keys::Address;
use utils::merkle;

use crate::account::PrunedAccount;
use crate::block::BlockError;
use crate::networks::NetworkId;
use crate::transaction::Transaction;

#[derive(Default, Clone, PartialEq, PartialOrd, Eq, Ord, Debug, Serialize, Deserialize)]
pub struct BlockBody {
    pub miner: Address,
    #[beserial(len_type(u8))]
    pub extra_data: Vec<u8>,
    #[beserial(len_type(u16))]
    pub transactions: Vec<Transaction>,
    #[beserial(len_type(u16))]
    pub pruned_accounts: Vec<PrunedAccount>,
}

impl SerializeContent for BlockBody {
    fn serialize_content<W: io::Write>(&self, writer: &mut W) -> io::Result<usize> { Ok(self.serialize(writer)?) }
}

impl Hash for BlockBody {
    fn hash<H: HashOutput>(&self) -> H {
        let mut vec: Vec<H> = Vec::with_capacity(2 + self.transactions.len() + self.pruned_accounts.len());
        vec.push(self.miner.hash());
        vec.push(self.extra_data.hash());
        for t in &self.transactions {
            vec.push(t.hash());
        }
        for p in &self.pruned_accounts {
            vec.push(p.hash());
        }
        return merkle::compute_root_from_hashes::<H>(&vec);
    }
}

#[allow(unreachable_code)]
impl BlockBody {
    pub fn verify(&self, block_height: u32, network_id: NetworkId) -> Result<(), BlockError> {
        let mut previous_tx: Option<&Transaction> = None;
        for tx in &self.transactions {
            // Ensure transactions are ordered and unique.
            if let Some(previous) = previous_tx {
                match previous.cmp_block_order(tx) {
                    Ordering::Equal => {
                        return Err(BlockError::DuplicateTransaction);
                    }
                    Ordering::Greater => {
                        return Err(BlockError::TransactionsNotOrdered);
                    }
                    _ => (),
                }
            }
            previous_tx = Some(tx);

            // Check that the transaction is within its validity window.
            if !tx.is_valid_at(block_height) {
                return Err(BlockError::ExpiredTransaction);
            }

            // Check intrinsic transaction invariants.
            if let Err(e) = tx.verify(network_id) {
                return Err(BlockError::InvalidTransaction(e));
            }
        }

        let mut previous_acc: Option<&PrunedAccount> = None;
        for acc in &self.pruned_accounts {
            // Ensure pruned accounts are ordered and unique.
            if let Some(previous) = previous_acc {
                match previous.cmp(acc) {
                    Ordering::Equal => {
                        return Err(BlockError::DuplicatePrunedAccount);
                    }
                    Ordering::Greater => {
                        return Err(BlockError::PrunedAccountsNotOrdered);
                    }
                    _ => (),
                }
            }
            previous_acc = Some(acc);

            // Check that the account is actually supposed to be pruned.
            if !acc.account.is_to_be_pruned() {
                return Err(BlockError::InvalidPrunedAccount);
            }
        }

        // Everything checks out.
        return Ok(());
    }
}
