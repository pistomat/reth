//! Output of execution.
use reth_db::{
    cursor::{DbCursorRO, DbCursorRW, DbDupCursorRO, DbDupCursorRW},
    models::{AccountBeforeTx, TransitionIdAddress},
    tables,
    transaction::{DbTx, DbTxMut},
    Error as DbError,
};
use reth_primitives::{
    Account, Address, Bytecode, Receipt, StorageEntry, TransitionId, H256, U256,
};
use std::collections::BTreeMap;

/// Storage for an account.
///
/// # Wiped Storage
///
/// The field `wiped` denotes whether any of the values contained in storage are valid or not; if
/// `wiped` is `true`, the storage should be considered empty.
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct Storage {
    /// Whether the storage was wiped or not.
    pub wiped: bool,
    /// The storage slots.
    pub storage: BTreeMap<U256, U256>,
}

/// Storage for an account with the old and new values for each slot.
/// TODO: Do we actually need (old, new) anymore, or is (old) sufficient? (Check the writes)
/// If we don't, we can unify this and [Storage].
pub type StorageChangeset = BTreeMap<U256, (U256, U256)>;

/// A change to the state of accounts or storage.
#[derive(Debug, Clone, Eq, PartialEq)]
pub enum Change {
    /// A new account was created.
    AccountCreated {
        /// The ID of the transition this change is a part of.
        id: TransitionId,
        /// The address of the account that was created.
        address: Address,
        /// The account.
        account: Account,
    },
    /// An existing account was changed.
    AccountChanged {
        /// The ID of the transition this change is a part of.
        id: TransitionId,
        /// The address of the account that was changed.
        address: Address,
        /// The account before the change.
        old: Account,
        /// The account after the change.
        new: Account,
    },
    /// Storage slots for an account were changed.
    StorageChanged {
        /// The ID of the transition this change is a part of.
        id: TransitionId,
        /// The address of the account associated with the storage slots.
        address: Address,
        /// The storage changeset.
        changeset: StorageChangeset,
    },
    /// Storage was wiped
    StorageWiped {
        /// The ID of the transition this change is a part of.
        id: TransitionId,
        /// The address of the account whose storage was wiped.
        address: Address,
    },
    /// An account was destroyed.
    ///
    /// This removes all of the information associated with the account. An accompanying
    /// [Change::StorageWiped] will also be present to mark the deletion of storage.
    // TODO: Note on state clear EIP
    AccountDestroyed {
        /// The ID of the transition this change is a part of.
        id: TransitionId,
        /// The address of the destroyed account.
        address: Address,
        /// The account before it was destroyed.
        old: Account,
    },
}

impl Change {
    /// Get the transition ID for the change
    pub fn transition_id(&self) -> TransitionId {
        match self {
            Change::AccountChanged { id, .. } |
            Change::AccountCreated { id, .. } |
            Change::StorageChanged { id, .. } |
            Change::StorageWiped { id, .. } |
            Change::AccountDestroyed { id, .. } => *id,
        }
    }

    /// Get the address of the account this change operates on.
    pub fn address(&self) -> Address {
        match self {
            Change::AccountChanged { address, .. } |
            Change::AccountCreated { address, .. } |
            Change::StorageChanged { address, .. } |
            Change::StorageWiped { address, .. } |
            Change::AccountDestroyed { address, .. } => *address,
        }
    }

    /// Set the transition ID of this change.
    pub fn set_transition_id(&mut self, new_id: TransitionId) {
        match self {
            Change::AccountChanged { ref mut id, .. } |
            Change::AccountCreated { ref mut id, .. } |
            Change::StorageChanged { ref mut id, .. } |
            Change::StorageWiped { ref mut id, .. } |
            Change::AccountDestroyed { ref mut id, .. } => {
                *id = new_id;
            }
        }
    }
}

/// The state of accounts after execution of one or more transactions, including receipts and new
/// bytecode.
///
/// The latest state can be found in `accounts`, `storage`, and `bytecode`. The receipts for the
/// transactions that lead to these changes can be found in `receipts`, and each change leading to
/// this state can be found in `changes`.
///
/// # Wiped Storage
///
/// The [Storage] type has a field, `wiped`, which denotes whether any of the values contained
/// in storage are valid or not; if `wiped` is `true`, the storage for the account should be
/// considered empty.
///
/// # Transitions
///
/// Each [Change] has an `id` field that marks what transition it is part of. Each transaction is
/// its own transition, but there may be 0 or 1 transitions associated with the block.
///
/// The block level transition includes:
///
/// - Block rewards
/// - Ommer rewards
/// - Withdrawals
/// - The irregular state change for the DAO hardfork
///
/// [PostState::finish_transition] should be called after every transaction, and after every block.
///
/// The first transaction executed and added to the [PostState] has a transition ID of 0, the next
/// one a transition ID of 1, and so on. If the [PostState] is for a single block, and the number of
/// transitions ([PostState::transitions_count]) is greater than the number of transactions in the
/// block, then the last transition is the block transition.
///
/// For multi-block [PostState]s it is not possible to figure out what transition ID maps on to a
/// transaction or a block.
///
/// # Shaving Allocations
///
/// Since most [PostState]s in reth are for multiple blocks it is better to pre-allocate capacity
/// for receipts and changes, which [PostState::new] does, and thus it (or
/// [PostState::with_tx_capacity]) should be preferred to using the [Default] implementation.
#[derive(Debug, Default, Clone)]
pub struct PostState {
    /// The ID of the current transition.
    current_transition_id: TransitionId,
    /// The state of all modified accounts after execution.
    ///
    /// If the value contained is `None`, then the account should be deleted.
    accounts: BTreeMap<Address, Option<Account>>,
    /// The state of all modified storage after execution
    ///
    /// If the contained [Storage] is marked as wiped, then all storage values should be cleared
    /// from the database.
    storage: BTreeMap<Address, Storage>,
    /// The changes to state that happened during execution
    changes: Vec<Change>,
    /// New code created during the execution
    bytecode: BTreeMap<H256, Bytecode>,
    /// The receipt(s) of the executed transaction(s).
    receipts: Vec<Receipt>,
}

/// Used to determine preallocation sizes of [PostState]'s internal [Vec]s. It denotes the number of
/// best-guess changes each transaction causes to state.
const BEST_GUESS_CHANGES_PER_TX: usize = 8;

/// How many [Change]s to preallocate for in [PostState].
///
/// This is just a guesstimate based on:
///
/// - Each block having ~200-300 transactions
/// - Each transaction having some amount of changes
const PREALLOC_CHANGES_SIZE: usize = 256 * BEST_GUESS_CHANGES_PER_TX;

// TODO: Reduce clones and deallocations
impl PostState {
    /// Create an empty [PostState].
    pub fn new() -> Self {
        Self { changes: Vec::with_capacity(PREALLOC_CHANGES_SIZE), ..Default::default() }
    }

    /// Create an empty [PostState] with pre-allocated space for a certain amount of transactions.
    pub fn with_tx_capacity(txs: usize) -> Self {
        Self {
            changes: Vec::with_capacity(txs * BEST_GUESS_CHANGES_PER_TX),
            receipts: Vec::with_capacity(txs),
            ..Default::default()
        }
    }

    /// Get the latest state of accounts.
    pub fn accounts(&self) -> &BTreeMap<Address, Option<Account>> {
        &self.accounts
    }

    /// Get the latest state of storage.
    pub fn storage(&self) -> &BTreeMap<Address, Storage> {
        &self.storage
    }

    /// Get the changes causing this [PostState].
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// Get the newly created bytecodes
    pub fn bytecode(&self) -> &BTreeMap<H256, Bytecode> {
        &self.bytecode
    }

    /// Get the receipts for the transactions executed to form this [PostState].
    pub fn receipts(&self) -> &[Receipt] {
        &self.receipts
    }

    /// Get the number of transitions causing this [PostState]
    pub fn transitions_count(&self) -> usize {
        self.current_transition_id as usize
    }

    /// Extend this [PostState] with the changes in another [PostState].
    pub fn extend(&mut self, other: PostState) {
        self.changes.reserve(other.changes.len());

        let mut next_transition_id = self.current_transition_id;
        for mut change in other.changes.into_iter() {
            next_transition_id = self.current_transition_id + change.transition_id();
            change.set_transition_id(next_transition_id);
            self.add_and_apply(change);
        }
        self.receipts.extend(other.receipts);
        self.bytecode.extend(other.bytecode);
        self.current_transition_id = next_transition_id;
    }

    /// Add a newly created account to the post-state.
    pub fn create_account(&mut self, address: Address, account: Account) {
        self.add_and_apply(Change::AccountCreated {
            id: self.current_transition_id,
            address,
            account,
        });
    }

    /// Add a changed account to the post-state.
    ///
    /// If the account also has changed storage values, [PostState::change_storage] should also be
    /// called.
    pub fn change_account(&mut self, address: Address, old: Account, new: Account) {
        self.add_and_apply(Change::AccountChanged {
            id: self.current_transition_id,
            address,
            old,
            new,
        });
    }

    /// Mark an account as destroyed.
    pub fn destroy_account(&mut self, address: Address, account: Account) {
        self.add_and_apply(Change::AccountDestroyed {
            id: self.current_transition_id,
            address,
            old: account,
        });
        self.add_and_apply(Change::StorageWiped { id: self.current_transition_id, address });
    }

    /// Add changed storage values to the post-state.
    pub fn change_storage(&mut self, address: Address, changeset: StorageChangeset) {
        self.add_and_apply(Change::StorageChanged {
            id: self.current_transition_id,
            address,
            changeset,
        });
    }

    /// Add new bytecode to the post-state.
    pub fn add_bytecode(&mut self, code_hash: H256, bytecode: Bytecode) {
        // TODO: Is this faster than just doing `.insert`?
        // Assumption: `insert` will override the value if present, but since the code hash for a
        // given bytecode will always be the same, we are overriding with the same value.
        //
        // In other words: if this entry already exists, replacing the bytecode will replace with
        // the same value, which is wasteful.
        self.bytecode.entry(code_hash).or_insert(bytecode);
    }

    /// Add a transaction receipt to the post-state.
    ///
    /// Transactions should always include their receipts in the post-state.
    pub fn add_receipt(&mut self, receipt: Receipt) {
        self.receipts.push(receipt);
    }

    /// Mark all prior changes as being part of one transition, and start a new one.
    pub fn finish_transition(&mut self) {
        self.current_transition_id += 1;
    }

    /// Add a new change, and apply its transformations to the current state
    fn add_and_apply(&mut self, change: Change) {
        match &change {
            Change::AccountCreated { address, account, .. } |
            Change::AccountChanged { address, new: account, .. } => {
                self.accounts.insert(*address, Some(*account));
            }
            Change::AccountDestroyed { address, .. } => {
                self.accounts.insert(*address, None);
            }
            Change::StorageChanged { address, changeset, .. } => {
                let storage = self.storage.entry(*address).or_default();
                storage.wiped = false;
                for (slot, (_, current_value)) in changeset {
                    storage.storage.insert(*slot, *current_value);
                }
            }
            Change::StorageWiped { address, .. } => {
                let storage = self.storage.entry(*address).or_default();
                storage.wiped = true;
            }
        }

        self.changes.push(change);
    }

    /// Write the post state to the database.
    pub fn write_to_db<'a, TX: DbTxMut<'a> + DbTx<'a>>(
        mut self,
        tx: &TX,
        first_transition_id: TransitionId,
    ) -> Result<(), DbError> {
        // Collect and sort changesets by their key to improve write performance
        let mut changesets = std::mem::take(&mut self.changes);
        changesets
            .sort_unstable_by_key(|changeset| (changeset.transition_id(), changeset.address()));

        // Partition changesets into account and storage changes
        let (account_changes, storage_changes): (Vec<Change>, Vec<Change>) =
            changesets.into_iter().partition(|changeset| {
                matches!(
                    changeset,
                    Change::AccountChanged { .. } |
                        Change::AccountCreated { .. } |
                        Change::AccountDestroyed { .. }
                )
            });

        // Write account changes
        let mut account_changeset_cursor = tx.cursor_dup_write::<tables::AccountChangeSet>()?;
        for changeset in account_changes.into_iter() {
            match changeset {
                Change::AccountDestroyed { id, address, old } |
                Change::AccountChanged { id, address, old, .. } => {
                    account_changeset_cursor.append_dup(
                        first_transition_id + id,
                        AccountBeforeTx { address, info: Some(old) },
                    )?;
                }
                Change::AccountCreated { id, address, .. } => {
                    account_changeset_cursor.append_dup(
                        first_transition_id + id,
                        AccountBeforeTx { address, info: None },
                    )?;
                }
                _ => unreachable!(),
            }
        }

        // Write storage changes
        let mut storages_cursor = tx.cursor_dup_write::<tables::PlainStorageState>()?;
        let mut storage_changeset_cursor = tx.cursor_dup_write::<tables::StorageChangeSet>()?;
        for changeset in storage_changes.into_iter() {
            match changeset {
                Change::StorageChanged { id, address, changeset } => {
                    let storage_id = TransitionIdAddress((first_transition_id + id, address));

                    for (key, (old_value, _)) in changeset {
                        storage_changeset_cursor.append_dup(
                            storage_id,
                            StorageEntry { key: H256(key.to_be_bytes()), value: old_value },
                        )?;
                    }
                }
                Change::StorageWiped { id, address } => {
                    let storage_id = TransitionIdAddress((first_transition_id + id, address));

                    if storages_cursor.seek_exact(address)?.is_some() {
                        while let Some(entry) = storages_cursor.next_dup_val()? {
                            storage_changeset_cursor.append_dup(storage_id, entry)?;
                        }
                    }
                }
                _ => unreachable!(),
            }
        }

        // Write new storage state
        for (address, storage) in self.storage.into_iter() {
            if storage.wiped {
                if storages_cursor.seek_exact(address)?.is_some() {
                    storages_cursor.delete_current_duplicates()?;
                }

                // If the storage is marked as wiped, it might still contain values. This is to
                // avoid deallocating where possible, but these values should not be written to the
                // database.
                continue
            }

            for (key, value) in storage.storage {
                let key = H256(key.to_be_bytes());
                if let Some(entry) = storages_cursor.seek_by_key_subkey(address, key)? {
                    if entry.key == key {
                        storages_cursor.delete_current()?;
                    }
                }

                if value != U256::ZERO {
                    storages_cursor.upsert(address, StorageEntry { key, value })?;
                }
            }
        }

        // Write new account state
        let mut accounts_cursor = tx.cursor_write::<tables::PlainAccountState>()?;
        for (address, account) in self.accounts.into_iter() {
            if let Some(account) = account {
                /*if has_state_clear_eip && account.is_empty() {
                    // TODO: seek and then delete?
                    continue
                }*/
                accounts_cursor.upsert(address, account)?;
            } else if accounts_cursor.seek_exact(address)?.is_some() {
                accounts_cursor.delete_current()?;
            }
        }

        // Write bytecode
        let mut bytecodes_cursor = tx.cursor_write::<tables::Bytecodes>()?;
        for (hash, bytecode) in self.bytecode.into_iter() {
            bytecodes_cursor.upsert(hash, bytecode)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use reth_db::{
        database::Database,
        mdbx::{test_utils, Env, EnvKind, WriteMap},
        transaction::DbTx,
    };
    use reth_primitives::H160;

    use super::*;

    /*#[test]
    fn apply_account_info_changeset() {
        let db: Arc<Env<WriteMap>> = test_utils::create_test_db(EnvKind::RW);
        let address = H160::zero();
        let tx_num = 0;
        let acc1 = Account { balance: U256::from(1), nonce: 2, bytecode_hash: Some(H256::zero()) };
        let acc2 = Account { balance: U256::from(3), nonce: 4, bytecode_hash: Some(H256::zero()) };

        let tx = db.tx_mut().unwrap();

        // check Changed changeset
        AccountInfoChangeSet::Changed { new: acc1, old: acc2 }
            .apply_to_db(&tx, address, tx_num, true)
            .unwrap();
        assert_eq!(
            tx.get::<tables::AccountChangeSet>(tx_num),
            Ok(Some(AccountBeforeTx { address, info: Some(acc2) }))
        );
        assert_eq!(tx.get::<tables::PlainAccountState>(address), Ok(Some(acc1)));

        AccountInfoChangeSet::Created { new: acc1 }
            .apply_to_db(&tx, address, tx_num, true)
            .unwrap();
        assert_eq!(
            tx.get::<tables::AccountChangeSet>(tx_num),
            Ok(Some(AccountBeforeTx { address, info: None }))
        );
        assert_eq!(tx.get::<tables::PlainAccountState>(address), Ok(Some(acc1)));

        // delete old value, as it is dupsorted
        tx.delete::<tables::AccountChangeSet>(tx_num, None).unwrap();

        AccountInfoChangeSet::Destroyed { old: acc2 }
            .apply_to_db(&tx, address, tx_num, true)
            .unwrap();
        assert_eq!(tx.get::<tables::PlainAccountState>(address), Ok(None));
        assert_eq!(
            tx.get::<tables::AccountChangeSet>(tx_num),
            Ok(Some(AccountBeforeTx { address, info: Some(acc2) }))
        );
    }*/
}
