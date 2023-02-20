#![allow(missing_docs)]
//! Main `t8n` command
//!
//! Runs an EVM state transition using Reth's executor module

use crate::dirs::{DbPath, PlatformPath};
use clap::Parser;
use ethers_core::types::TxHash;
use reth_db::database::Database;
use reth_executor::{
    executor::{test_utils::InMemoryStateProvider, Executor},
    revm_wrap::{State, SubState},
    AccountState, Database as RevmDatabase,
};
use reth_primitives::{
    Address, Block, BlockNumber, Bytes, ChainSpecBuilder, Hardfork, Header, H256, U256, U64,
};
use reth_provider::{
    BlockProvider, HistoricalStateProvider, LatestStateProvider, ShareableDatabase, Transaction,
};
use reth_rpc_types as rpc;
use reth_staged_sync::utils::init::init_db;
use serde::{Deserialize, Serialize, Serializer};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    path::PathBuf,
    sync::Arc,
};

/// `reth prestate` command
#[derive(Debug, Parser)]
pub struct Command {
    block: BlockNumber,
    tx_hash: TxHash,

    /// The path to the database folder.
    ///
    /// Defaults to the OS-specific data directory:
    ///
    /// - Linux: `$XDG_DATA_HOME/reth/db` or `$HOME/.local/share/reth/db`
    /// - Windows: `{FOLDERID_RoamingAppData}/reth/db`
    /// - macOS: `$HOME/Library/Application Support/reth/db`
    #[arg(long, value_name = "PATH", verbatim_doc_comment, default_value_t)]
    db: PlatformPath<DbPath>,
}

impl Command {
    /// Execute `prestate` command
    // TODO: Clean up
    pub async fn execute(&self) -> eyre::Result<()> {
        let spec = ChainSpecBuilder::mainnet().build();

        let db = Arc::new(init_db(&self.db)?);
        let s = ShareableDatabase::new(db.clone(), spec.clone());
        let mut block =
            s.block(self.block.into())?.ok_or_else(|| eyre::eyre!("block not found"))?;
        let transition_id = {
            let tx = Transaction::new(&db).unwrap();
            tx.get_block_transition(self.block).unwrap()
        };

        let mut filtered = Vec::new();
        let mut target = None;
        for tx in block.body.drain(..) {
            if tx.hash == self.tx_hash.into() {
                target = Some(tx);
                break
            }
            filtered.push(tx);
        }
        let target = target.ok_or_else(|| eyre::eyre!("tx not found in block"))?;
        block.body = filtered;

        let state_provider = HistoricalStateProvider::new(db.tx().unwrap(), transition_id);
        let mut substate = SubState::new(State::new(state_provider));
        let mut executor = Executor::new(&spec, &mut substate);
        // todo: TD
        let _ = executor.execute_transactions(&block, U256::ZERO, None);
        let result = executor.execute_transaction(
            &target,
            target
                .try_ecrecovered()
                .ok_or_else(|| eyre::eyre!("could not recover sender"))?
                .signer(),
        );

        let all_accounts = substate.accounts.clone();
        println!("Found {} accounts in database, filtering...", all_accounts.len());

        let accounts: HashMap<Address, PrestateAccount> = all_accounts
            .iter()
            .filter(|(_, account)| !matches!(account.account_state, AccountState::NotExisting))
            .map(|(address, account)| {
                let code = substate
                    .code_by_hash(account.info.code_hash)
                    .ok()
                    .as_ref()
                    .filter(|code| !code.is_empty())
                    .map(|code| Bytes(code.bytes().clone()));
                (
                    *address,
                    PrestateAccount {
                        balance: account.info.balance,
                        nonce: account.info.nonce.into(),
                        storage: account
                            .storage
                            .iter()
                            .map(|(a, b)| (a.clone(), b.clone()))
                            .collect(),
                        code,
                    },
                )
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&accounts).unwrap());

        Ok(())
    }
}

/// The state of an account prior to execution of the target transaction.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrestateAccount {
    /// The balance of the account
    pub balance: U256,
    /// The nonce of the account
    pub nonce: U64,
    /// The storage slots of the account.
    ///
    /// Note: This only includes the storage slots that were read or written to during execution of
    /// the transactions.
    #[serde(serialize_with = "geth_alloc_compat")]
    pub storage: HashMap<U256, U256>,
    /// The bytecode of the account.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<Bytes>,
}

fn geth_alloc_compat<S>(value: &HashMap<U256, U256>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.collect_map(
        value.iter().map(|(k, v)| (format!("0x{:0>64x}", k), format!("0x{:0>64x}", v))),
    )
}
