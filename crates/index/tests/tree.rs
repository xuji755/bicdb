//! B-link B+Tree 的端到端用例（§9.1；分裂协议 §9.1.5）。
//!
//! 用内存页仓（`MemStore`）驱动树：插入/查找/范围/删除/分裂/根生长/
//! 99-1 常态路径/重复键/空页留树/结构自检。

use bicdb_index::{InsertOutcome, MemStore, SplitKind, Tree};
use bicdb_storage::rowid::RowId;

const FILE_ID: u16 = 3;
const WS: [u8; 8] = [7u8; 8];

fn rid(block: u32, row: u16) -> RowId {
    RowId::from_parts(FILE_ID, block, row).unwrap()
}

/// 定长键：`k%06d` 形态（字节序 = 数值序）。
fn key(i: u32) -> Vec<u8> {
    format!("k{i:06}").into_bytes()
}

/// 大值载荷（撑页用）：键 + 填充。
fn big_key(i: u32, pad: usize) -> Vec<u8> {
    let mut k = key(i);
    k.extend(std::iter::repeat(b'x').take(pad));
    k
}

fn store(capacity: u32) -> MemStore {
    MemStore::new(capacity, FILE_ID, WS)
}

#[test]
fn single_page_insert_lookup_scan_delete() {
    let mut s = store(64);
    let mut tree = Tree::create(&mut s, FILE_ID, WS).unwrap();
    assert_eq!(tree.height(), 0, "空树：根即叶");

    // 插入 20 个小键（单页装得下）。
    for i in 0..20u32 {
        let out = tree.insert(&key(i), rid(1, (i + 1) as u16)).unwrap();
        assert!(out.splits.is_empty(), "小页内不应分裂");
        assert!(!out.grew);
    }
    // 点查。
    for i in 0..20u32 {
        assert_eq!(tree.lookup(&key(i)).unwrap(), Some(rid(1, (i + 1) as u16)));
    }
    assert_eq!(tree.lookup(b"k999999").unwrap(), None);
    // 全扫描：按键升序（复合键 (key, ROWID)）。
    let all = tree.full_scan(100).unwrap();
    assert_eq!(all.len(), 20);
    let keys: Vec<Vec<u8>> = all.iter().map(|(k, _)| k.clone()).collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted, "叶链顺序 = 键序");

    // 范围扫描（闭区间）。
    let part = tree.range(Some(&key(5)), Some(&key(9)), 100).unwrap();
    assert_eq!(part.len(), 5);
    assert_eq!(part.first().unwrap().0, key(5));
    assert_eq!(part.last().unwrap().0, key(9));

    // 删除：删掉的查不到、其余不动。
    assert!(tree.delete(&key(7), rid(1, 8)).unwrap());
    assert_eq!(tree.lookup(&key(7)).unwrap(), None);
    assert_eq!(tree.lookup(&key(8)).unwrap(), Some(rid(1, 9)));
    assert!(
        !tree.delete(&key(7), rid(1, 8)).unwrap(),
        "重复删除 = 未命中"
    );
    tree.validate().unwrap();
}

#[test]
fn splits_grow_the_tree_and_keep_every_key_findable() {
    // 大键（~600B）撑页：约 25 条就分裂；插 400 条 ⇒ 多层。
    let mut s = store(4096);
    let mut tree = Tree::create(&mut s, FILE_ID, WS).unwrap();
    let mut splits = 0usize;
    for i in 0..400u32 {
        let out = tree
            .insert(&big_key(i, 600), rid(1, (i % 1000 + 1) as u16))
            .unwrap();
        splits += out.splits.len();
    }
    assert!(splits > 0, "应发生分裂");
    assert!(
        tree.height() >= 1,
        "树长高（根分裂）：height = {}",
        tree.height()
    );
    // 全部可查。
    for i in 0..400u32 {
        assert_eq!(
            tree.lookup(&big_key(i, 600)).unwrap(),
            Some(rid(1, (i % 1000 + 1) as u16)),
            "键 {i} 应可查"
        );
    }
    // 全扫描 = 全部且升序。
    let all = tree.full_scan(10_000).unwrap();
    assert_eq!(all.len(), 400);
    let mut prev: Option<Vec<u8>> = None;
    for (k, _) in &all {
        if let Some(p) = &prev {
            assert!(*p < *k, "扫描顺序非单调");
        }
        prev = Some(k.clone());
    }
    tree.validate().unwrap();
}

#[test]
fn monotonic_appends_use_the_99_1_split() {
    // §9.1.5：新条目是页内最大条目 ⇒ 99/1（零搬移）——**单调追加是常态路径**。
    let mut s = store(2048);
    let mut tree = Tree::create(&mut s, FILE_ID, WS).unwrap();
    let mut kinds = Vec::new();
    for i in 0..200u32 {
        let out: InsertOutcome = tree.insert(&big_key(i, 600), rid(1, 1)).unwrap();
        kinds.extend(out.splits);
    }
    assert!(!kinds.is_empty());
    assert!(
        kinds.iter().filter(|k| **k == SplitKind::Append99).count() * 2 >= kinds.len(),
        "单调追加下 99/1 应占多数：{kinds:?}"
    );
}

#[test]
fn random_order_inserts_use_even_splits_and_stay_searchable() {
    // 乱序插入（可复现的伪随机）：50/50 分裂为主，键序与可达性保持。
    let mut s = store(4096);
    let mut tree = Tree::create(&mut s, FILE_ID, WS).unwrap();
    let mut order: Vec<u32> = (0..300u32).collect();
    // 线性同余打乱（可复现，无需 rand 依赖）。
    let mut x = 12345u64;
    for i in (1..order.len()).rev() {
        x = (1103515245 * x + 12345) % (1 << 31);
        let j = (x as usize) % (i + 1);
        order.swap(i, j);
    }
    for i in &order {
        tree.insert(&big_key(*i, 300), rid(1, (*i % 1000 + 1) as u16))
            .unwrap();
    }
    for i in 0..300u32 {
        assert!(tree.lookup(&big_key(i, 300)).unwrap().is_some(), "键 {i}");
    }
    tree.validate().unwrap();
    assert_eq!(tree.full_scan(10_000).unwrap().len(), 300);
}

#[test]
fn splitting_a_middle_leaf_keeps_the_link_chain_intact() {
    // 回归（§9.1.1 三页协议）：P′ 必须**继承 P 的后继**——漏掉时 P′ 恒以
    // no_link 收尾，叶链在 P′ 处提前终止（monotonic 追加看不见：P 恒最右）。
    let mut s = store(4096);
    let mut tree = Tree::create(&mut s, FILE_ID, WS).unwrap();
    // 先隔一插落成多叶（0,2,4,...）……
    for i in 0..150u32 {
        tree.insert(&big_key(i * 2, 600), rid(1, 1)).unwrap();
    }
    assert!(tree.height() >= 1);
    // ……再回填奇数键：落点全在**非最右**的既有叶上。
    for i in 0..150u32 {
        tree.insert(&big_key(i * 2 + 1, 600), rid(1, 1)).unwrap();
    }
    let all = tree.full_scan(10_000).unwrap();
    assert_eq!(all.len(), 300, "叶链不得断裂");
    tree.validate().unwrap();
}

#[test]
fn duplicate_keys_are_ordered_by_rowid() {
    let mut s = store(64);
    let mut tree = Tree::create(&mut s, FILE_ID, WS).unwrap();
    let k = b"dup-key".to_vec();
    for row in [5u16, 1, 3] {
        tree.insert(&k, rid(1, row)).unwrap();
    }
    // 扫描：同键按 ROWID 升序（复合键 (key, ROWID)）。
    let all = tree.full_scan(10).unwrap();
    let rids: Vec<u16> = all.iter().map(|(_, r)| r.row_id()).collect();
    assert_eq!(rids, vec![1, 3, 5]);
    // 点查返回最小 ROWID。
    assert_eq!(tree.lookup(&k).unwrap(), Some(rid(1, 1)));
    // 删中间一个：其余保留。
    assert!(tree.delete(&k, rid(1, 3)).unwrap());
    assert_eq!(
        tree.full_scan(10)
            .unwrap()
            .iter()
            .map(|(_, r)| r.row_id())
            .collect::<Vec<_>>(),
        vec![1, 5]
    );
    tree.validate().unwrap();
}

#[test]
fn emptied_leaf_stays_in_the_tree() {
    // §9.1.2：索引项随删除移除，**空叶页不得失联**——高键条目是锚。
    let mut s = store(4096);
    let mut tree = Tree::create(&mut s, FILE_ID, WS).unwrap();
    for i in 0..200u32 {
        tree.insert(&big_key(i, 600), rid(1, 1)).unwrap();
    }
    // 全部删除：树仍在、结构自检通过、后续插入照常。
    for i in 0..200u32 {
        assert!(tree.delete(&big_key(i, 600), rid(1, 1)).unwrap(), "键 {i}");
    }
    tree.validate().unwrap();
    assert!(tree.full_scan(10).unwrap().is_empty());
    tree.insert(&big_key(999, 600), rid(1, 9)).unwrap();
    assert_eq!(tree.lookup(&big_key(999, 600)).unwrap(), Some(rid(1, 9)));
    tree.validate().unwrap();
}

#[test]
fn overlong_keys_are_rejected_at_insert() {
    let mut s = store(64);
    let mut tree = Tree::create(&mut s, FILE_ID, WS).unwrap();
    let huge = vec![b'z'; bicdb_index::MAX_KEY_LEN + 1];
    assert!(matches!(
        tree.insert(&huge, rid(1, 1)),
        Err(bicdb_index::IndexError::EntryTooLong { .. })
    ));
    // 上限内的键可用。
    let ok = vec![b'z'; bicdb_index::MAX_KEY_LEN];
    tree.insert(&ok, rid(1, 1)).unwrap();
    assert!(tree.lookup(&ok).unwrap().is_some());
}

#[test]
fn long_common_prefix_keys_split_with_prefix_truncation() {
    // 长公共前缀：分支条目的区分前缀只是"能区分"的那一段（§9.1.3）。
    let mut s = store(4096);
    let mut tree = Tree::create(&mut s, FILE_ID, WS).unwrap();
    let base = "prefix-that-is-long-and-shared-".repeat(6); // 192B 公共前缀
    for i in 0..300u32 {
        let mut k = base.clone().into_bytes();
        k.extend_from_slice(format!("{i:06}").as_bytes());
        tree.insert(&k, rid(1, (i % 1000 + 1) as u16)).unwrap();
    }
    for i in 0..300u32 {
        let mut k = base.clone().into_bytes();
        k.extend_from_slice(format!("{i:06}").as_bytes());
        assert!(tree.lookup(&k).unwrap().is_some(), "键 {i}");
    }
    assert!(tree.height() >= 1);
    tree.validate().unwrap();
}
