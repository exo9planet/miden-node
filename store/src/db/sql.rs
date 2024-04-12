//! Wrapper functions for SQL statements.

use std::{borrow::Cow, rc::Rc};

use miden_node_proto::domain::accounts::{AccountInfo, AccountSummary, AccountUpdateDetails};
use miden_objects::{
    accounts::{Account, AccountDelta},
    crypto::{hash::rpo::RpoDigest, merkle::MerklePath},
    notes::{NoteId, Nullifier},
    transaction::AccountDetails,
    utils::serde::{Deserializable, Serializable},
    BlockHeader,
};
use rusqlite::{
    params,
    types::{Value, ValueRef},
    Connection, Transaction,
};

use super::{Note, NoteCreated, NullifierInfo, Result, StateSyncUpdate};
use crate::{
    errors::{DatabaseError, StateSyncError},
    types::{AccountId, BlockNumber},
};

// ACCOUNT QUERIES
// ================================================================================================

/// Select all accounts from the DB using the given [Connection].
///
/// # Returns
///
/// A vector with accounts, or an error.
pub fn select_accounts(conn: &mut Connection) -> Result<Vec<AccountInfo>> {
    let mut stmt = conn.prepare(
        "
        SELECT
            account_id,
            account_hash,
            block_num,
            details
        FROM
            accounts
        ORDER BY
            block_num ASC;
    ",
    )?;
    let mut rows = stmt.query([])?;

    let mut accounts = vec![];
    while let Some(row) = rows.next()? {
        accounts.push(account_info_from_row(row)?)
    }
    Ok(accounts)
}

/// Select all account hashes from the DB using the given [Connection].
///
/// # Returns
///
/// The vector with the account id and corresponding hash, or an error.
pub fn select_account_hashes(conn: &mut Connection) -> Result<Vec<(AccountId, RpoDigest)>> {
    let mut stmt =
        conn.prepare("SELECT account_id, account_hash FROM accounts ORDER BY block_num ASC;")?;
    let mut rows = stmt.query([])?;

    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let account_id = column_value_as_u64(row, 0)?;
        let account_hash_data = row.get_ref(1)?.as_blob()?;
        let account_hash = RpoDigest::read_from_bytes(account_hash_data)?;

        result.push((account_id, account_hash));
    }

    Ok(result)
}

/// Select [AccountSummary] from the DB using the given [Connection], given that the account
/// update was done between `(block_start, block_end]`.
///
/// # Returns
///
/// The vector of [AccountSummary] with the matching accounts.
pub fn select_accounts_by_block_range(
    conn: &mut Connection,
    block_start: BlockNumber,
    block_end: BlockNumber,
    account_ids: &[AccountId],
) -> Result<Vec<AccountSummary>> {
    let account_ids: Vec<Value> = account_ids.iter().copied().map(u64_to_value).collect();

    let mut stmt = conn.prepare(
        "
        SELECT
            account_id,
            account_hash,
            block_num
        FROM
            accounts
        WHERE
            block_num > ?1 AND
            block_num <= ?2 AND
            account_id IN rarray(?3)
        ORDER BY
            block_num ASC
    ",
    )?;

    let mut rows = stmt.query(params![block_start, block_end, Rc::new(account_ids)])?;

    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(account_hash_update_from_row(row)?)
    }

    Ok(result)
}

/// Select the latest account details by account id from the DB using the given [Connection].
///
/// # Returns
///
/// The latest account details, or an error.
pub fn select_account(conn: &mut Connection, account_id: AccountId) -> Result<AccountInfo> {
    let mut stmt = conn.prepare(
        "
        SELECT
            account_id,
            account_hash,
            block_num,
            details
        FROM
            accounts
        WHERE
            account_id = ?1;
    ",
    )?;

    let mut rows = stmt.query(params![u64_to_value(account_id)])?;
    let row = rows.next()?.ok_or(DatabaseError::AccountNotFoundInDb(account_id))?;

    account_info_from_row(row)
}

/// Inserts or updates accounts to the DB using the given [Transaction].
///
/// # Returns
///
/// The number of affected rows.
///
/// # Note
///
/// The [Transaction] object is not consumed. It's up to the caller to commit or rollback the
/// transaction.
pub fn upsert_accounts(
    transaction: &Transaction,
    accounts: &[AccountUpdateDetails],
    block_num: BlockNumber,
) -> Result<usize> {
    let mut upsert_stmt = transaction.prepare(
        "INSERT OR REPLACE INTO accounts (account_id, account_hash, block_num, details) VALUES (?1, ?2, ?3, ?4);",
    )?;
    let mut select_details_stmt =
        transaction.prepare("SELECT details FROM accounts WHERE account_id = ?1;")?;

    let mut count = 0;
    for update in accounts.iter() {
        let account_id = update.account_id.into();
        let full_account = match &update.details {
            None => None,
            Some(AccountDetails::Full(account)) => {
                debug_assert_eq!(account_id, u64::from(account.id()));

                if account.hash() != update.final_state_hash {
                    return Err(DatabaseError::ApplyBlockFailedAccountHashesMismatch {
                        calculated: account.hash(),
                        expected: update.final_state_hash,
                    });
                }

                Some(Cow::Borrowed(account))
            },
            Some(AccountDetails::Delta(delta)) => {
                let mut rows = select_details_stmt.query(params![u64_to_value(account_id)])?;
                let Some(row) = rows.next()? else {
                    return Err(DatabaseError::AccountNotFoundInDb(account_id));
                };

                let account =
                    apply_delta(account_id, &row.get_ref(0)?, delta, &update.final_state_hash)?;

                Some(Cow::Owned(account))
            },
        };

        let inserted = upsert_stmt.execute(params![
            u64_to_value(account_id),
            update.final_state_hash.to_bytes(),
            block_num,
            full_account.as_ref().map(|account| account.to_bytes()),
        ])?;

        debug_assert_eq!(inserted, 1);

        count += inserted;
    }

    Ok(count)
}

// NULLIFIER QUERIES
// ================================================================================================

/// Insert nullifiers to the DB using the given [Transaction].
///
/// # Returns
///
/// The number of affected rows.
///
/// # Note
///
/// The [Transaction] object is not consumed. It's up to the caller to commit or rollback the
/// transaction.
pub fn insert_nullifiers_for_block(
    transaction: &Transaction,
    nullifiers: &[Nullifier],
    block_num: BlockNumber,
) -> Result<usize> {
    let mut stmt = transaction.prepare(
        "INSERT INTO nullifiers (nullifier, nullifier_prefix, block_num) VALUES (?1, ?2, ?3);",
    )?;

    let mut count = 0;
    for nullifier in nullifiers.iter() {
        count +=
            stmt.execute(params![nullifier.to_bytes(), get_nullifier_prefix(nullifier), block_num])?
    }
    Ok(count)
}

/// Select all nullifiers from the DB using the given [Connection].
///
/// # Returns
///
/// A vector with nullifiers and the block height at which they were created, or an error.
pub fn select_nullifiers(conn: &mut Connection) -> Result<Vec<(Nullifier, BlockNumber)>> {
    let mut stmt =
        conn.prepare("SELECT nullifier, block_num FROM nullifiers ORDER BY block_num ASC;")?;
    let mut rows = stmt.query([])?;

    let mut result = vec![];
    while let Some(row) = rows.next()? {
        let nullifier_data = row.get_ref(0)?.as_blob()?;
        let nullifier = Nullifier::read_from_bytes(nullifier_data)?;
        let block_number = row.get(1)?;
        result.push((nullifier, block_number));
    }
    Ok(result)
}

/// Select nullifiers created between `(block_start, block_end]` that also match the
/// `nullifier_prefixes` filter using the given [Connection].
///
/// Each value of the `nullifier_prefixes` is only the 16 most significant bits of the nullifier of
/// interest to the client. This hides the details of the specific nullifier being requested.
///
/// # Returns
///
/// A vector of [NullifierInfo] with the nullifiers and the block height at which they were
/// created, or an error.
pub fn select_nullifiers_by_block_range(
    conn: &mut Connection,
    block_start: BlockNumber,
    block_end: BlockNumber,
    nullifier_prefixes: &[u32],
) -> Result<Vec<NullifierInfo>> {
    let nullifier_prefixes: Vec<Value> =
        nullifier_prefixes.iter().copied().map(u32_to_value).collect();

    let mut stmt = conn.prepare(
        "
        SELECT
            nullifier,
            block_num
        FROM
            nullifiers
        WHERE
            block_num > ?1 AND
            block_num <= ?2 AND
            nullifier_prefix IN rarray(?3)
        ORDER BY
            block_num ASC
    ",
    )?;

    let mut rows = stmt.query(params![block_start, block_end, Rc::new(nullifier_prefixes)])?;

    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let nullifier_data = row.get_ref(0)?.as_blob()?;
        let nullifier = Nullifier::read_from_bytes(nullifier_data)?;
        let block_num = row.get(1)?;
        result.push(NullifierInfo { nullifier, block_num });
    }
    Ok(result)
}

// NOTE QUERIES
// ================================================================================================

/// Select all notes from the DB using the given [Connection].
///
///
/// # Returns
///
/// A vector with notes, or an error.
pub fn select_notes(conn: &mut Connection) -> Result<Vec<Note>> {
    let mut stmt = conn.prepare(
        "
        SELECT
            block_num,
            batch_index,
            note_index,
            note_hash,
            note_type,
            sender,
            tag,
            merkle_path,
            details
        FROM
            notes
        ORDER BY
            block_num ASC;
        ",
    )?;
    let mut rows = stmt.query([])?;

    let mut notes = vec![];
    while let Some(row) = rows.next()? {
        let note_id_data = row.get_ref(3)?.as_blob()?;
        let note_id = RpoDigest::read_from_bytes(note_id_data)?;

        let merkle_path_data = row.get_ref(7)?.as_blob()?;
        let merkle_path = MerklePath::read_from_bytes(merkle_path_data)?;

        let details_data = row.get_ref(8)?.as_blob_or_null()?;
        let details = details_data.map(<Vec<u8>>::read_from_bytes).transpose()?;

        notes.push(Note {
            block_num: row.get(0)?,
            note_created: NoteCreated {
                batch_index: row.get(1)?,
                note_index: row.get(2)?,
                note_id,
                note_type: row.get::<_, u8>(4)?.try_into()?,
                sender: column_value_as_u64(row, 5)?,
                tag: row.get(6)?,
                details,
            },
            merkle_path,
        })
    }
    Ok(notes)
}

/// Insert notes to the DB using the given [Transaction].
///
/// # Returns
///
/// The number of affected rows.
///
/// # Note
///
/// The [Transaction] object is not consumed. It's up to the caller to commit or rollback the
/// transaction.
pub fn insert_notes(transaction: &Transaction, notes: &[Note]) -> Result<usize> {
    let mut stmt = transaction.prepare(
        "
        INSERT INTO
        notes
        (
            block_num,
            batch_index,
            note_index,
            note_hash,
            note_type,
            sender,
            tag,
            merkle_path,
            details
        )
        VALUES
        (
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9
        );",
    )?;

    let mut count = 0;
    for note in notes.iter() {
        let details = note.note_created.details.as_ref().map(|details| details.to_bytes());

        count += stmt.execute(params![
            note.block_num,
            note.note_created.batch_index,
            note.note_created.note_index,
            note.note_created.note_id.to_bytes(),
            note.note_created.note_type as u8,
            u64_to_value(note.note_created.sender),
            note.note_created.tag,
            note.merkle_path.to_bytes(),
            details
        ])?;
    }

    Ok(count)
}

/// Select notes matching the tag and account_ids search criteria using the given [Connection].
///
/// # Returns
///
/// - Empty vector if no tag created after `block_num` match `tags` or `account_ids`.
/// - Otherwise, notes which the 16 high bits match `tags`, or the `sender` is one of the
///   `account_ids`.
///
/// # Note
///
/// This method returns notes from a single block. To fetch all notes up to the chain tip,
/// multiple requests are necessary.
pub fn select_notes_since_block_by_tag_and_sender(
    conn: &mut Connection,
    tags: &[u32],
    account_ids: &[AccountId],
    block_num: BlockNumber,
) -> Result<Vec<Note>> {
    let tags: Vec<Value> = tags.iter().copied().map(u32_to_value).collect();
    let account_ids: Vec<Value> = account_ids.iter().copied().map(u64_to_value).collect();

    let mut stmt = conn.prepare(
        "
        SELECT
            block_num,
            batch_index,
            note_index,
            note_hash,
            note_type,
            sender,
            tag,
            merkle_path,
            details
        FROM
            notes
        WHERE
            -- find the next block which contains at least one note with a matching tag
            block_num = (
                SELECT
                    block_num
                FROM
                    notes
                WHERE
                    (tag IN rarray(?1) OR sender IN rarray(?2)) AND
                    block_num > ?3
                ORDER BY
                    block_num ASC
                LIMIT
                    1
            ) AND
            -- filter the block's notes and return only the ones matching the requested tags
            (tag IN rarray(?1) OR sender IN rarray(?2));
    ",
    )?;
    let mut rows = stmt.query(params![Rc::new(tags), Rc::new(account_ids), block_num])?;

    let mut res = Vec::new();
    while let Some(row) = rows.next()? {
        let block_num = row.get(0)?;
        let batch_index = row.get(1)?;
        let note_index = row.get(2)?;
        let note_id_data = row.get_ref(3)?.as_blob()?;
        let note_id = RpoDigest::read_from_bytes(note_id_data)?;
        let note_type = row.get::<_, u8>(4)?.try_into()?;
        let sender = column_value_as_u64(row, 5)?;
        let tag = row.get(6)?;
        let merkle_path_data = row.get_ref(7)?.as_blob()?;
        let merkle_path = MerklePath::read_from_bytes(merkle_path_data)?;
        let details_data = row.get_ref(8)?.as_blob_or_null()?;
        let details = details_data.map(<Vec<u8>>::read_from_bytes).transpose()?;

        let note = Note {
            block_num,
            note_created: NoteCreated {
                batch_index,
                note_index,
                note_id,
                note_type,
                sender,
                tag,
                details,
            },
            merkle_path,
        };
        res.push(note);
    }
    Ok(res)
}

/// Select Note's matching the NoteId using the given [Connection].
///
/// # Returns
///
/// - Empty vector if no matching `note`.
/// - Otherwise, notes which `note_hash` matches the `NoteId` as bytes.
pub fn select_notes_by_id(conn: &mut Connection, note_ids: &[NoteId]) -> Result<Vec<Note>> {
    let note_ids: Vec<Value> = note_ids.iter().map(|id| id.to_bytes().into()).collect();

    let mut stmt = conn.prepare(
        "
        SELECT
            block_num,
            batch_index,
            note_index,
            note_hash,
            note_type,
            sender,
            tag,
            merkle_path,
            details
        FROM
            notes
        WHERE
            note_hash IN rarray(?1)
        ",
    )?;
    let mut rows = stmt.query(params![Rc::new(note_ids)])?;

    let mut notes = Vec::new();
    while let Some(row) = rows.next()? {
        let note_id_data = row.get_ref(3)?.as_blob()?;
        let note_id = NoteId::read_from_bytes(note_id_data)?;

        let merkle_path_data = row.get_ref(7)?.as_blob()?;
        let merkle_path = MerklePath::read_from_bytes(merkle_path_data)?;

        let details_data = row.get_ref(8)?.as_blob_or_null()?;
        let details = details_data.map(<Vec<u8>>::read_from_bytes).transpose()?;

        notes.push(Note {
            block_num: row.get(0)?,
            note_created: NoteCreated {
                batch_index: row.get(1)?,
                note_index: row.get(2)?,
                details,
                note_id: note_id.into(),
                note_type: row.get::<_, u8>(4)?.try_into()?,
                sender: column_value_as_u64(row, 5)?,
                tag: row.get(6)?,
            },
            merkle_path,
        })
    }
    Ok(notes)
}

// BLOCK CHAIN QUERIES
// ================================================================================================

/// Insert a [BlockHeader] to the DB using the given [Transaction].
///
/// # Returns
///
/// The number of affected rows.
///
/// # Note
///
/// The [Transaction] object is not consumed. It's up to the caller to commit or rollback the
/// transaction.
pub fn insert_block_header(transaction: &Transaction, block_header: &BlockHeader) -> Result<usize> {
    let mut stmt = transaction
        .prepare("INSERT INTO block_headers (block_num, block_header) VALUES (?1, ?2);")?;
    Ok(stmt.execute(params![block_header.block_num(), block_header.to_bytes()])?)
}

/// Select a [BlockHeader] from the DB by its `block_num` using the given [Connection].
///
/// # Returns
///
/// When `block_number` is [None], the latest block header is returned. Otherwise, the block with the
/// given block height is returned.
pub fn select_block_header_by_block_num(
    conn: &mut Connection,
    block_number: Option<BlockNumber>,
) -> Result<Option<BlockHeader>> {
    let mut stmt;
    let mut rows = match block_number {
        Some(block_number) => {
            stmt = conn.prepare("SELECT block_header FROM block_headers WHERE block_num = ?1")?;
            stmt.query([block_number])?
        },
        None => {
            stmt = conn.prepare(
                "SELECT block_header FROM block_headers ORDER BY block_num DESC LIMIT 1",
            )?;
            stmt.query([])?
        },
    };

    match rows.next()? {
        Some(row) => {
            let data = row.get_ref(0)?.as_blob()?;
            Ok(Some(BlockHeader::read_from_bytes(data)?))
        },
        None => Ok(None),
    }
}

/// Select all block headers from the DB using the given [Connection].
///
/// # Returns
///
/// A vector of [BlockHeader] or an error.
pub fn select_block_headers(conn: &mut Connection) -> Result<Vec<BlockHeader>> {
    let mut stmt =
        conn.prepare("SELECT block_header FROM block_headers ORDER BY block_num ASC;")?;
    let mut rows = stmt.query([])?;
    let mut result = vec![];
    while let Some(row) = rows.next()? {
        let block_header_data = row.get_ref(0)?.as_blob()?;
        let block_header = BlockHeader::read_from_bytes(block_header_data)?;
        result.push(block_header);
    }

    Ok(result)
}

// STATE SYNC
// ================================================================================================

/// Loads the state necessary for a state sync.
pub fn get_state_sync(
    conn: &mut Connection,
    block_num: BlockNumber,
    account_ids: &[AccountId],
    note_tag_prefixes: &[u32],
    nullifier_prefixes: &[u32],
) -> Result<StateSyncUpdate, StateSyncError> {
    let notes = select_notes_since_block_by_tag_and_sender(
        conn,
        note_tag_prefixes,
        account_ids,
        block_num,
    )?;

    let (block_header, chain_tip) = if !notes.is_empty() {
        let block_header = select_block_header_by_block_num(conn, Some(notes[0].block_num))?
            .ok_or(StateSyncError::EmptyBlockHeadersTable)?;
        let tip = select_block_header_by_block_num(conn, None)?
            .ok_or(StateSyncError::EmptyBlockHeadersTable)?;

        (block_header, tip.block_num())
    } else {
        let block_header = select_block_header_by_block_num(conn, None)?
            .ok_or(StateSyncError::EmptyBlockHeadersTable)?;

        let block_num = block_header.block_num();
        (block_header, block_num)
    };

    let account_updates =
        select_accounts_by_block_range(conn, block_num, block_header.block_num(), account_ids)?;

    let nullifiers = select_nullifiers_by_block_range(
        conn,
        block_num,
        block_header.block_num(),
        nullifier_prefixes,
    )?;

    Ok(StateSyncUpdate {
        notes,
        block_header,
        chain_tip,
        account_updates,
        nullifiers,
    })
}

// APPLY BLOCK
// ================================================================================================

/// Updates the DB with the state of a new block.
///
/// # Returns
///
/// The number of affected rows in the DB.
pub fn apply_block(
    transaction: &Transaction,
    block_header: &BlockHeader,
    notes: &[Note],
    nullifiers: &[Nullifier],
    accounts: &[AccountUpdateDetails],
) -> Result<usize> {
    let mut count = 0;
    count += insert_block_header(transaction, block_header)?;
    count += insert_notes(transaction, notes)?;
    count += upsert_accounts(transaction, accounts, block_header.block_num())?;
    count += insert_nullifiers_for_block(transaction, nullifiers, block_header.block_num())?;
    Ok(count)
}

// UTILITIES
// ================================================================================================

/// Returns the high 16 bits of the provided nullifier.
pub(crate) fn get_nullifier_prefix(nullifier: &Nullifier) -> u32 {
    (nullifier.most_significant_felt().as_int() >> 48) as u32
}

/// Converts a `u64` into a [Value].
///
/// Sqlite uses `i64` as its internal representation format. Note that the `as` operator performs a
/// lossless conversion from `u64` to `i64`.
fn u64_to_value(v: u64) -> Value {
    Value::Integer(v as i64)
}

/// Converts a `u32` into a [Value].
///
/// Sqlite uses `i64` as its internal representation format.
fn u32_to_value(v: u32) -> Value {
    let v: i64 = v.into();
    Value::Integer(v)
}

/// Gets a `u64` value from the database.
///
/// Sqlite uses `i64` as its internal representation format, and so when retrieving
/// we need to make sure we cast as `u64` to get the original value
fn column_value_as_u64<I: rusqlite::RowIndex>(
    row: &rusqlite::Row<'_>,
    index: I,
) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    Ok(value as u64)
}

/// Constructs `AccountSummary` from the row of `accounts` table.
///
/// Note: field ordering must be the same, as in `accounts` table!
fn account_hash_update_from_row(row: &rusqlite::Row<'_>) -> Result<AccountSummary> {
    let account_id = column_value_as_u64(row, 0)?;
    let account_hash_data = row.get_ref(1)?.as_blob()?;
    let account_hash = RpoDigest::read_from_bytes(account_hash_data)?;
    let block_num = row.get(2)?;

    Ok(AccountSummary {
        account_id: account_id.try_into()?,
        account_hash,
        block_num,
    })
}

/// Constructs `AccountInfo` from the row of `accounts` table.
///
/// Note: field ordering must be the same, as in `accounts` table!
fn account_info_from_row(row: &rusqlite::Row<'_>) -> Result<AccountInfo> {
    let update = account_hash_update_from_row(row)?;

    let details = row.get_ref(3)?.as_blob_or_null()?;
    let details = details.map(Account::read_from_bytes).transpose()?;

    Ok(AccountInfo { summary: update, details })
}

/// Deserializes account and applies account delta.
fn apply_delta(
    account_id: u64,
    value: &ValueRef<'_>,
    delta: &AccountDelta,
    final_state_hash: &RpoDigest,
) -> Result<Account, DatabaseError> {
    let account = value.as_blob_or_null()?;
    let account = account.map(Account::read_from_bytes).transpose()?;

    let Some(mut account) = account else {
        return Err(DatabaseError::AccountNotOnChain(account_id));
    };

    account.apply_delta(delta)?;

    let actual_hash = account.hash();
    if &actual_hash != final_state_hash {
        return Err(DatabaseError::ApplyBlockFailedAccountHashesMismatch {
            calculated: actual_hash,
            expected: *final_state_hash,
        });
    }

    Ok(account)
}
