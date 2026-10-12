//! 多选：按节点下标记住选中项，与列表的行号无关，重新排序、大小变化后依然有效。
//!
//! 选中集合是一张位图（按节点下标，O(1) 判断）加一个有序列表（只有选中的项，遍历和清空都只和选中数有关），
//! 不会因为树有几百万个节点就每帧扫一遍。点击规则与资源管理器一致：
//!
//! - 单击：只选这一项，它同时成为锚点和焦点；
//! - Ctrl+单击：切换这一项，锚点移到这里；
//! - Shift+单击：按当前列表的顺序选中锚点到这一项的范围（替换原有选择，锚点不变）；
//! - Ctrl+Shift+单击：把这个范围加进原有选择。
//!
//! 删除和合计大小前先"规范化"：一个文件夹和它里面的东西都被选中时只保留文件夹，避免重复计算、重复删除。
//! 规范化沿每个选中项向上找祖先，开销是选中数 × 深度，与整棵树的大小无关。

use sweep_scan::Tree;

#[derive(Default, Clone, Debug)]
pub struct Selection {
    /// 节点下标 → 是否选中。按需增长（重新扫描子树会在末尾追加节点）。
    bits: Vec<u64>,
    /// 选中的节点，按加入的先后。
    items: Vec<u32>,
    /// Shift 范围选择的起点。
    anchor: Option<u32>,
    /// 键盘光标：方向键从这里移动，Enter / F5 等单项操作作用于它。
    focus: Option<u32>,
    /// 每次改动加一，供缓存合计用。
    generation: u64,
}

impl Selection {
    pub fn contains(&self, n: u32) -> bool {
        self.bits
            .get(n as usize / 64)
            .is_some_and(|w| w & (1 << (n % 64)) != 0)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 选中的节点，按加入的先后。
    pub fn items(&self) -> &[u32] {
        &self.items
    }

    #[cfg(test)]
    pub fn anchor(&self) -> Option<u32> {
        self.anchor
    }

    pub fn focus(&self) -> Option<u32> {
        self.focus
    }

    /// 焦点项，且它确实被选中（单项操作用它）。
    pub fn focused_selected(&self) -> Option<u32> {
        self.focus.filter(|&f| self.contains(f))
    }

    /// 只有一项被选中时返回它。
    pub fn single(&self) -> Option<u32> {
        match self.items.as_slice() {
            [n] => Some(*n),
            _ => None,
        }
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn insert(&mut self, n: u32) -> bool {
        let w = n as usize / 64;
        if w >= self.bits.len() {
            self.bits.resize(w + 1, 0);
        }
        let bit = 1u64 << (n % 64);
        if self.bits[w] & bit != 0 {
            return false;
        }
        self.bits[w] |= bit;
        self.items.push(n);
        self.generation += 1;
        true
    }

    fn remove(&mut self, n: u32) -> bool {
        if !self.contains(n) {
            return false;
        }
        self.bits[n as usize / 64] &= !(1u64 << (n % 64));
        self.items.retain(|&i| i != n);
        self.generation += 1;
        true
    }

    /// 清空选中项（只清选中的那些位，不碰整张位图）。保留焦点，方向键还能从原处继续。
    pub fn clear(&mut self) {
        for &n in &self.items {
            self.bits[n as usize / 64] &= !(1u64 << (n % 64));
        }
        self.items.clear();
        self.anchor = None;
        self.generation += 1;
    }

    /// 只选这一项，它成为锚点和焦点。
    pub fn set_single(&mut self, n: u32) {
        self.clear();
        self.insert(n);
        self.anchor = Some(n);
        self.focus = Some(n);
    }

    /// Ctrl+单击：切换这一项，锚点和焦点移到这里。
    pub fn toggle(&mut self, n: u32) {
        if !self.remove(n) {
            self.insert(n);
        }
        self.anchor = Some(n);
        self.focus = Some(n);
    }

    /// 把 `to` 行设为焦点，并选中锚点到它的范围（`order(i)` 是第 i 行的节点）。
    /// `additive` 为 false 时替换原有选择。锚点不在列表里时退化为只选 `to`。
    pub fn extend_to(
        &mut self,
        order: impl Fn(usize) -> u32,
        len: usize,
        to: usize,
        additive: bool,
    ) {
        if to >= len {
            return;
        }
        let target = order(to);
        let Some(from) = self
            .anchor
            .and_then(|a| (0..len).position(|i| order(i) == a))
        else {
            if additive {
                self.toggle(target);
            } else {
                self.set_single(target);
            }
            return;
        };
        let anchor = self.anchor;
        if !additive {
            self.clear();
        }
        let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
        for i in lo..=hi {
            self.insert(order(i));
        }
        self.anchor = anchor;
        self.focus = Some(target);
        self.generation += 1;
    }

    /// 在列表的第 `index` 行上单击（带修饰键）。
    pub fn click(
        &mut self,
        order: impl Fn(usize) -> u32,
        len: usize,
        index: usize,
        ctrl: bool,
        shift: bool,
    ) {
        if index >= len {
            return;
        }
        match (ctrl, shift) {
            (_, true) => self.extend_to(order, len, index, ctrl),
            (true, false) => self.toggle(order(index)),
            (false, false) => self.set_single(order(index)),
        }
    }

    /// 只选这些项（Ctrl+A）。焦点不在其中时移到第一项。
    pub fn select_all(&mut self, nodes: impl IntoIterator<Item = u32>) {
        self.clear();
        for n in nodes {
            self.insert(n);
        }
        if self.focus.is_none_or(|f| !self.contains(f)) {
            self.focus = self.items.first().copied();
        }
        self.anchor = self.focus;
    }

    /// 去掉不满足 `keep` 的项（例如已从树上删除的节点）。
    pub fn retain(&mut self, mut keep: impl FnMut(u32) -> bool) {
        let before = self.items.len();
        let bits = &mut self.bits;
        self.items.retain(|&n| {
            let k = keep(n);
            if !k {
                bits[n as usize / 64] &= !(1u64 << (n % 64));
            }
            k
        });
        if self.items.len() != before {
            self.generation += 1;
        }
        if self.anchor.is_some_and(|a| !self.contains(a)) {
            self.anchor = None;
        }
    }

    /// 只把焦点移到 `n`，不改选中项（删除后光标退回还在的祖先）。
    pub fn set_focus(&mut self, n: Option<u32>) {
        self.focus = n;
    }

    /// 规范化后的选中项：去掉祖先也被选中的项。顺序与选中的先后一致。
    pub fn normalized(&self, tree: &Tree) -> Vec<u32> {
        self.items
            .iter()
            .copied()
            .filter(|&n| !tree.ancestors(n).skip(1).any(|a| self.contains(a)))
            .collect()
    }

    /// 要删除的项：先去掉不能删除的（`deletable` 为 false），再在剩下的里面规范化。
    /// 返回 (要删的项, 被排除的不能删除的项数)。祖先被选中但不能删除（例如驱动器根目录）时，
    /// 选中的子项照常保留。
    pub fn deletion_set(
        &self,
        tree: &Tree,
        deletable: impl Fn(&Tree, u32) -> bool,
    ) -> (Vec<u32>, usize) {
        let mut excluded = 0;
        let mut out = Vec::new();
        for &n in &self.items {
            if !deletable(tree, n) {
                excluded += 1;
                continue;
            }
            let covered = tree
                .ancestors(n)
                .skip(1)
                .any(|a| self.contains(a) && deletable(tree, a));
            if !covered {
                out.push(n);
            }
        }
        (out, excluded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::deletable;
    use sweep_scan::{Entry, flags};

    fn file(size: u64) -> Entry {
        Entry {
            size,
            alloc: size,
            mtime: 0,
            flags: 0,
        }
    }

    fn dir() -> Entry {
        Entry {
            flags: flags::DIR,
            ..Default::default()
        }
    }

    /// C:\ ─ $MFT(meta 500) ─ a\ (x 10, y 30) ─ b\ (c\ (z 100)) ─ top 5
    struct Fixture {
        t: Tree,
        root: u32,
        mft: u32,
        a: u32,
        x: u32,
        y: u32,
        b: u32,
        c: u32,
        z: u32,
        top: u32,
    }

    fn fixture() -> Fixture {
        let mut t = Tree::new(r"C:\");
        let root = t.root;
        let mft = t.push(
            root,
            "$MFT",
            Entry {
                flags: flags::META,
                ..file(500)
            },
        );
        let a = t.push(root, "a", dir());
        let x = t.push(a, "x", file(10));
        let y = t.push(a, "y", file(30));
        let b = t.push(root, "b", dir());
        let c = t.push(b, "c", dir());
        let z = t.push(c, "z", file(100));
        let top = t.push(root, "top", file(5));
        t.finish();
        Fixture {
            t,
            root,
            mft,
            a,
            x,
            y,
            b,
            c,
            z,
            top,
        }
    }

    #[test]
    fn click_toggle_and_ranges() {
        let rows = [10u32, 11, 12, 13, 14, 15];
        let order = |i: usize| rows[i];
        let mut s = Selection::default();
        s.click(order, rows.len(), 1, false, false);
        assert_eq!(s.items(), &[11]);
        // Shift：锚点 11 到 13。
        s.click(order, rows.len(), 3, false, true);
        assert_eq!(s.items(), &[11, 12, 13]);
        assert_eq!((s.anchor(), s.focus()), (Some(11), Some(13)));
        // 再次 Shift 到上方：替换为 10..=11，锚点不变。
        s.click(order, rows.len(), 0, false, true);
        assert_eq!(s.items(), &[10, 11]);
        // Ctrl：切换 14，锚点移到 14。
        s.click(order, rows.len(), 4, true, false);
        assert_eq!(s.items(), &[10, 11, 14]);
        assert_eq!(s.anchor(), Some(14));
        // Ctrl+Shift：追加 14..=15，保留原有。
        s.click(order, rows.len(), 5, true, true);
        assert_eq!(s.items(), &[10, 11, 14, 15]);
        // Ctrl 再点一次取消。
        s.click(order, rows.len(), 0, true, false);
        assert_eq!(s.items(), &[11, 14, 15]);
        assert!(!s.contains(10) && s.contains(15));
        // 单击回到只选一项。
        s.click(order, rows.len(), 2, false, false);
        assert_eq!(s.items(), &[12]);
        assert_eq!(s.single(), Some(12));
    }

    #[test]
    fn range_survives_reorder_and_missing_anchor() {
        let mut s = Selection::default();
        s.set_single(12);
        // 列表重新排序后，范围按新的顺序计算。
        let rows = [14u32, 12, 10, 11];
        s.extend_to(|i| rows[i], rows.len(), 3, false);
        assert_eq!(s.items(), &[12, 10, 11]);
        // 锚点不在列表里：Shift 退化为单选。
        let other = [1u32, 2, 3];
        s.extend_to(|i| other[i], other.len(), 2, false);
        assert_eq!(s.items(), &[3]);
    }

    #[test]
    fn select_all_clear_and_retain() {
        let mut s = Selection::default();
        s.set_single(5);
        s.select_all(100_000..200_000);
        assert_eq!(s.len(), 100_000);
        assert_eq!(s.focus(), Some(100_000));
        assert!(s.contains(150_000) && !s.contains(5));
        let g = s.generation();
        s.retain(|n| n % 2 == 0);
        assert_eq!(s.len(), 50_000);
        assert!(!s.contains(150_001) && s.generation() != g);
        s.clear();
        assert!(s.is_empty() && !s.contains(150_000));
        assert_eq!(s.focus(), Some(100_000), "清空保留键盘光标");
        assert_eq!(s.focused_selected(), None);
    }

    #[test]
    fn normalizes_nested_selection() {
        let f = fixture();
        let mut s = Selection::default();
        for n in [f.z, f.x, f.b, f.a, f.c, f.top] {
            s.toggle(n);
        }
        // z、c 在 b 里，x 在 a 里。
        assert_eq!(s.normalized(&f.t), vec![f.b, f.a, f.top]);
        let total: u64 = s.normalized(&f.t).iter().map(|&n| f.t.value(n)).sum();
        assert_eq!(total, 145);
        let (del, excluded) = s.deletion_set(&f.t, deletable);
        assert_eq!(del, vec![f.b, f.a, f.top]);
        assert_eq!(excluded, 0);
    }

    #[test]
    fn deletion_ignores_undeletable_nodes() {
        let f = fixture();
        let mut s = Selection::default();
        // 根、元文件、以及根下的普通项：根不能删，但它下面选中的项照常删除。
        for n in [f.root, f.mft, f.y, f.top] {
            s.toggle(n);
        }
        let (del, excluded) = s.deletion_set(&f.t, deletable);
        assert_eq!(del, vec![f.y, f.top]);
        assert_eq!(excluded, 2);
        // 只选了不能删除的：什么都不剩。
        s.set_single(f.root);
        s.toggle(f.mft);
        assert_eq!(s.deletion_set(&f.t, deletable), (vec![], 2));
        // 规范化（算合计）不看能否删除：根盖住一切。
        assert_eq!(s.normalized(&f.t), vec![f.root]);
    }

    #[test]
    fn deleted_nodes_are_not_deletable() {
        let mut f = fixture();
        let mut s = Selection::default();
        s.toggle(f.x);
        s.toggle(f.y);
        f.t.remove(f.x);
        let (del, excluded) = s.deletion_set(&f.t, deletable);
        assert_eq!((del, excluded), (vec![f.y], 1));
        s.retain(|n| !f.t.node(n).has(flags::DELETED));
        assert_eq!(s.items(), &[f.y]);
        let _ = (f.b, f.c);
    }
}
