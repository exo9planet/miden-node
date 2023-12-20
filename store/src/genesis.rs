use std::path::PathBuf;

use miden_crypto::merkle::{EmptySubtreeRoots, MerkleError, MmrPeaks, SimpleSmt, TieredSmt};
use miden_objects::{
    accounts::Account,
    notes::NOTE_LEAF_DEPTH,
    utils::serde::{ByteReader, ByteWriter, Deserializable, DeserializationError, Serializable},
    BlockHeader, Digest,
};
use once_cell::sync::Lazy;

use crate::config::{APP, ORG};

// FIXME: This is a duplicate of the constant in `store::state`
pub(crate) const ACCOUNT_DB_DEPTH: u8 = 64;

/// Default path at which the genesis file will be written to
pub static DEFAULT_GENESIS_FILE_PATH: Lazy<PathBuf> = Lazy::new(|| {
    directories::ProjectDirs::from("", ORG, APP)
        .map(|d| d.data_local_dir().join("genesis.dat"))
        // fallback to current dir
        .unwrap_or_default()
        .as_path()
        .to_str()
        .expect("path only contains UTF-8 characters")
        .into()
});

/// Represents the state at genesis, which will be used to derive the genesis block.
pub struct GenesisState {
    pub accounts: Vec<Account>,
    pub version: u64,
    pub timestamp: u64,
}

impl GenesisState {
    pub fn new(
        accounts: Vec<Account>,
        version: u64,
        timestamp: u64,
    ) -> Self {
        Self {
            accounts,
            version,
            timestamp,
        }
    }

    /// Returns the block header and the account SMT
    pub fn into_block_parts(self) -> Result<(BlockHeader, SimpleSmt), MerkleError> {
        let account_smt = SimpleSmt::with_leaves(
            ACCOUNT_DB_DEPTH,
            self.accounts
                .into_iter()
                .map(|account| (account.id().into(), account.hash().into())),
        )?;

        let block_header = BlockHeader::new(
            Digest::default(),
            1_u32,
            MmrPeaks::new(0, Vec::new()).unwrap().hash_peaks(),
            account_smt.root(),
            TieredSmt::default().root(),
            *EmptySubtreeRoots::entry(NOTE_LEAF_DEPTH, 0),
            Digest::default(),
            Digest::default(),
            self.version.into(),
            self.timestamp.into(),
        );

        Ok((block_header, account_smt))
    }
}

// SERIALIZATION
// ================================================================================================

impl Serializable for GenesisState {
    fn write_into<W: ByteWriter>(
        &self,
        target: &mut W,
    ) {
        assert!(self.accounts.len() <= u64::MAX as usize, "too many accounts in GenesisState");
        target.write_u64(self.accounts.len() as u64);

        for account in self.accounts.iter() {
            account.write_into(target);
        }

        target.write_u64(self.version);
        target.write_u64(self.timestamp);
    }
}

impl Deserializable for GenesisState {
    fn read_from<R: ByteReader>(source: &mut R) -> Result<Self, DeserializationError> {
        let num_accounts = source.read_u64()? as usize;
        let accounts = Account::read_batch_from(source, num_accounts)?;

        let version = source.read_u64()?;
        let timestamp = source.read_u64()?;

        Ok(Self::new(accounts, version, timestamp))
    }
}