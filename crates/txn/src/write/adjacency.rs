//! Adjacency record and chain writes over the existing transactional mechanism.
//! Endpoint resolution/ordered vertex locks and graph index maintenance belong
//! to the graph access caller; this layer never commits independently.

use super::{
    append_undo_via_pool, ensure_itl_entry, lock_and_occupy, occupy_itl, require_pool_bound,
    write_page_change, ItlAcquire, Txn, TxnError,
};
use bicdb_storage::adjacency::{self as layout, AdjacencyError, EDGE_ITL_OFFSET, NEXT_PAGE_OFFSET};
use bicdb_storage::buffer::{BufferKey, BufferPool};
use bicdb_storage::heap::InsertPolicy;
use bicdb_storage::page::{Page, PageType, PAGE_SIZE};
use bicdb_storage::rowid::RowId;
use bicdb_storage::undo::{UndoChain, UndoOp, UndoPayload, UndoRecord};
use bicdb_wal::group::GroupWriter;

fn snapshot(pool: &BufferPool<'_>, block: BufferKey) -> Result<([u8; PAGE_SIZE], Page), TxnError> {
    let guard = pool.pin(block)?;
    let before = *guard.as_bytes();
    let local = Page::from_bytes(Box::new(before));
    layout::header(&local)?;
    Ok((before, local))
}
fn identity(block: BufferKey, ordinal: u16) -> Result<RowId, TxnError> {
    Ok(RowId::from_parts(
        block.rdba.file_id(),
        block.rdba.block_id(),
        ordinal,
    )?)
}
// A relationship that fills its data page must still fit a complete old-value
// undo record. Validate the existing undo format before changing ITL or data.
fn preflight_old_value(block: BufferKey, bytes: &[u8]) -> Result<(), TxnError> {
    let record = UndoRecord {
        prev: None,
        op: UndoOp::Update,
        flags: 0,
        rowid: identity(block, 1)?,
        payload: UndoPayload::Update {
            old_itl_slot: bytes[EDGE_ITL_OFFSET],
            patches: vec![(0, bytes.to_vec())],
        },
    };
    let mut probe = Page::new(PageType::Undo, block.workspace, block.rdba.file_id(), 0);
    bicdb_storage::undo::put_record(&mut probe, &record)?;
    Ok(())
}

/// Append one identity in order, recording stable-slot undo before data redo.
#[allow(clippy::too_many_arguments)]
pub fn insert(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    block: BufferKey,
    bytes: &[u8],
    policy: &InsertPolicy,
) -> Result<RowId, TxnError> {
    require_pool_bound(chain, pool)?;
    let (before, mut local) = snapshot(pool, block)?;
    layout::decode(bytes)?;
    preflight_old_value(block, bytes)?;
    let mut probe = Page::from_bytes(Box::new(before));
    let probe_slot = match ensure_itl_entry(&mut probe, chain, txn.txn_id, policy.itl_max)? {
        ItlAcquire::Existing(slot) | ItlAcquire::Fresh { slot, .. } => slot,
    };
    let mut probe_bytes = bytes.to_vec();
    probe_bytes[EDGE_ITL_OFFSET] = probe_slot as u8;
    layout::append(&mut probe, &probe_bytes)?;
    let (slot, _) = occupy_itl(pool, log, chain, txn, &mut local, block, policy.itl_max)?;
    let mut patched = bytes.to_vec();
    patched[EDGE_ITL_OFFSET] = slot as u8;
    let ordinal = layout::append(&mut local, &patched)?;
    let rid = identity(block, ordinal)?;
    append_undo_via_pool(
        pool,
        log,
        chain,
        txn,
        UndoOp::Insert,
        rid,
        UndoPayload::None,
    )?;
    write_page_change(
        pool,
        log,
        txn.raw(),
        block,
        &before,
        local.as_bytes(),
        false,
    )?;
    Ok(rid)
}
/// Delete one edge; old bytes, stable directory ordinal and ITL are undoable.
#[allow(clippy::too_many_arguments)]
pub fn delete(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    block: BufferKey,
    ordinal: u16,
    policy: &InsertPolicy,
) -> Result<(), TxnError> {
    require_pool_bound(chain, pool)?;
    let (before, mut local) = snapshot(pool, block)?;
    let old = layout::record(&local, ordinal)?.to_vec();
    preflight_old_value(block, &old)?;
    let (slot, _) = lock_and_occupy(
        pool,
        log,
        chain,
        txn,
        &mut local,
        block,
        ordinal,
        policy.itl_max,
    )?;
    let offset = local
        .slot(ordinal as usize - 1)
        .ok_or(AdjacencyError::Missing)?
        .offset() as usize;
    local.as_bytes_mut()[offset + EDGE_ITL_OFFSET] = slot as u8;
    layout::delete(&mut local, ordinal)?;
    append_undo_via_pool(
        pool,
        log,
        chain,
        txn,
        UndoOp::Delete,
        identity(block, ordinal)?,
        UndoPayload::FullRow(old),
    )?;
    write_page_change(
        pool,
        log,
        txn.raw(),
        block,
        &before,
        local.as_bytes(),
        false,
    )?;
    Ok(())
}
/// Update variable data without changing identity/destination or directory ordinal.
#[allow(clippy::too_many_arguments)]
pub fn update(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    block: BufferKey,
    ordinal: u16,
    bytes: &[u8],
    policy: &InsertPolicy,
) -> Result<(), TxnError> {
    require_pool_bound(chain, pool)?;
    let (before, mut local) = snapshot(pool, block)?;
    let old = layout::record(&local, ordinal)?.to_vec();
    layout::decode(bytes)?;
    preflight_old_value(block, bytes)?;
    preflight_old_value(block, &old)?;
    let mut probe = Page::from_bytes(Box::new(before));
    let probe_slot = match ensure_itl_entry(&mut probe, chain, txn.txn_id, policy.itl_max)? {
        ItlAcquire::Existing(slot) | ItlAcquire::Fresh { slot, .. } => slot,
    };
    let mut probe_bytes = bytes.to_vec();
    probe_bytes[EDGE_ITL_OFFSET] = probe_slot as u8;
    layout::update(&mut probe, ordinal, &probe_bytes)?;
    let (slot, _) = lock_and_occupy(
        pool,
        log,
        chain,
        txn,
        &mut local,
        block,
        ordinal,
        policy.itl_max,
    )?;
    let mut patched = bytes.to_vec();
    patched[EDGE_ITL_OFFSET] = slot as u8;
    layout::update(&mut local, ordinal, &patched)?;
    append_undo_via_pool(
        pool,
        log,
        chain,
        txn,
        UndoOp::Update,
        identity(block, ordinal)?,
        UndoPayload::Update {
            old_itl_slot: old[EDGE_ITL_OFFSET],
            patches: vec![(0, old)],
        },
    )?;
    write_page_change(
        pool,
        log,
        txn.raw(),
        block,
        &before,
        local.as_bytes(),
        false,
    )?;
    Ok(())
}
/// Append a successor page to a previously unlinked tail. Metadata undo targets
/// a page-address ROWID; body-relative offsets survive later ITL-header growth.
#[allow(clippy::too_many_arguments)]
pub fn link(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    chain: &mut UndoChain<'_, '_>,
    txn: &mut Txn,
    block: BufferKey,
    next: RowId,
    policy: &InsertPolicy,
) -> Result<(), TxnError> {
    require_pool_bound(chain, pool)?;
    let (before, mut local) = snapshot(pool, block)?;
    if layout::header(&local)?.next.is_some() {
        return Err(AdjacencyError::IdentityChanged.into());
    }
    let mut probe = Page::from_bytes(Box::new(before));
    layout::set_next(&mut probe, Some(next))?;
    occupy_itl(pool, log, chain, txn, &mut local, block, policy.itl_max)?;
    layout::set_next(&mut local, Some(next))?;
    append_undo_via_pool(
        pool,
        log,
        chain,
        txn,
        UndoOp::Update,
        RowId::page_address(block.rdba.file_id(), block.rdba.block_id())?,
        UndoPayload::Update {
            old_itl_slot: 0xFF,
            patches: vec![(NEXT_PAGE_OFFSET as u32, vec![0; 6])],
        },
    )?;
    // Link-only writes also leave a page transaction trace. Existing fresh ITL
    // records already do this; ensure the occupied slot is part of the image.
    write_page_change(
        pool,
        log,
        txn.raw(),
        block,
        &before,
        local.as_bytes(),
        false,
    )?;
    Ok(())
}
/// Compact physical extents while keeping every stable slot and old-value byte.
/// No logical undo is necessary: all existing undo resolves through the directory.
pub fn compact(
    pool: &BufferPool<'_>,
    log: &mut GroupWriter<'_, '_>,
    txn: &Txn,
    block: BufferKey,
) -> Result<usize, TxnError> {
    let (before, mut local) = snapshot(pool, block)?;
    let reclaimed = layout::compact_retaining_undo(&mut local)?;
    if before.as_slice() != local.as_bytes() {
        write_page_change(
            pool,
            log,
            txn.raw(),
            block,
            &before,
            local.as_bytes(),
            false,
        )?;
    }
    Ok(reclaimed)
}
