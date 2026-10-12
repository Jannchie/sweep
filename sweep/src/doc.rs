//! 一次扫描的分析结果与界面状态：树、展开的目录、选中项、矩形树图的根，以及在后台算出的派生数据
//! （扩展名统计与配色、最大的文件、清理建议、搜索结果）。
//!
//! 树放在 `Arc` 里与后台线程共享。修改（删除后摘除节点、重新扫描子树）只在没有后台线程持有它时
//! 原地进行；否则先排队，下一帧再试，绝不在 UI 线程上复制整棵树。

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

use eframe::egui;
use sweep_scan::{Engine, NONE, Tree, flags};

use crate::cleanup::{self, Suggestion};
use crate::selection::Selection;

/// 派生数据（扩展名统计、最大文件、清理建议），对应某个数据版本。
pub struct Derived {
    pub version: u64,
    /// (扩展名下标, 大小, 文件数)，大的在前。
    pub exts: Vec<(u16, u64, u64)>,
    /// 扩展名下标 → 调色板下标（前 12 名），其余为 `u8::MAX`。
    pub colors: Arc<Vec<u8>>,
    pub largest: Vec<u32>,
}

pub enum Mutation {
    /// 已经删掉了，从树上摘除。
    Remove(Vec<u32>),
    /// 用重新扫描的结果替换这个目录的内容。
    Graft(u32, Box<Tree>),
}

/// 选中项的合计（缓存，选中项或数据变了才重算）。
#[derive(Clone, Copy, Default)]
pub struct SelSummary {
    /// 选中的项数（未规范化，即用户看到的高亮行数）。
    pub count: usize,
    /// 规范化后的合计大小（文件夹和它里面的项同时选中时不重复计算）。
    pub total: u64,
    /// 能删除的项数（规范化之后）。
    pub deletable: usize,
}

/// 每次扫描得到的 Doc 各有一个编号，后台删除完成时据此判断节点下标是否还对应同一棵树。
static NEXT_DOC: AtomicU64 = AtomicU64::new(1);

pub struct Doc {
    pub id: u64,
    pub tree: Arc<Tree>,
    pub target: String,
    pub engine: Engine,
    pub elapsed: Duration,
    /// 读取 MFT 失败、改用目录遍历时 MFT 那边的错误。
    pub mft_fallback: Option<String>,
    /// 数据版本：树每改一次加一。
    pub version: u64,
    pub expanded: HashSet<u32>,
    /// 树形列表当前显示的行：(节点, 缩进层级)。
    pub rows: Vec<(u32, u16)>,
    rows_gen: Option<u64>,
    pub selection: Selection,
    summary: Option<((u64, u64), SelSummary)>,
    /// 矩形树图当前的根。
    pub zoom: u32,
    pub highlight_ext: Option<u16>,
    pub derived: Option<Derived>,
    derived_rx: Option<Receiver<Derived>>,
    derived_requested: Option<u64>,
    pub suggestions: Option<Vec<Suggestion>>,
    suggest_rx: Option<Receiver<Vec<Suggestion>>>,
    suggest_requested: Option<u64>,
    pub search: Search,
    pending: Vec<Mutation>,
    /// 切换大小口径：后台复制一份树重新排序后换回来。
    basis_rx: Option<Receiver<Tree>>,
    /// 下一帧滚动到这一行。
    pub scroll_to: Option<usize>,
}

#[derive(Default)]
pub struct Search {
    pub query: String,
    pub results: Vec<u32>,
    /// 结果对应的 (查询, 数据版本)。
    done: Option<(String, u64)>,
    rx: Option<Receiver<(String, u64, Vec<u32>)>>,
    running: Option<(String, u64)>,
}

impl Search {
    pub fn active(&self) -> bool {
        !self.query.trim().is_empty()
    }

    pub fn busy(&self) -> bool {
        self.running.is_some()
    }
}

impl Doc {
    pub fn new(out: sweep_scan::Outcome, target: String) -> Self {
        let tree = Arc::new(out.tree);
        let root = tree.root;
        let mut selection = Selection::default();
        selection.set_single(root);
        Self {
            id: NEXT_DOC.fetch_add(1, Ordering::Relaxed),
            target,
            engine: out.engine,
            elapsed: out.elapsed,
            mft_fallback: out.mft_fallback,
            version: 1,
            expanded: HashSet::from([root]),
            rows: Vec::new(),
            rows_gen: None,
            selection,
            summary: None,
            zoom: root,
            highlight_ext: None,
            derived: None,
            derived_rx: None,
            derived_requested: None,
            suggestions: None,
            suggest_rx: None,
            suggest_requested: None,
            search: Search::default(),
            pending: Vec::new(),
            basis_rx: None,
            scroll_to: None,
            tree,
        }
    }

    /// 每帧调用：应用排队的修改、收取后台结果、按需发起后台计算。
    pub fn update(&mut self, ctx: &egui::Context) {
        self.apply_pending(ctx);
        if let Some(rx) = &self.basis_rx
            && let Ok(t) = rx.try_recv()
        {
            self.tree = Arc::new(t);
            self.basis_rx = None;
            self.bump();
        }
        if let Some(rx) = &self.derived_rx
            && let Ok(d) = rx.try_recv()
        {
            self.derived_rx = None;
            if d.version == self.version {
                self.derived = Some(d);
            }
        }
        if self.derived_requested != Some(self.version) && self.derived_rx.is_none() {
            self.spawn_derived(ctx);
        }
        if let Some(rx) = &self.suggest_rx
            && let Ok(s) = rx.try_recv()
        {
            self.suggest_rx = None;
            self.suggestions = Some(s);
        }
        self.poll_search(ctx);
        if self.rows_gen != Some(self.version) {
            self.rebuild_rows();
        }
    }

    /// 数据变了：版本加一，派生数据、行列表、搜索结果都会在之后重新计算。
    fn bump(&mut self) {
        self.version += 1;
        self.rows_gen = None;
    }

    pub fn invalidate_rows(&mut self) {
        self.rows_gen = None;
    }

    fn rebuild_rows(&mut self) {
        let t = &self.tree;
        let mut rows = Vec::new();
        let mut stack = vec![(t.root, 0u16)];
        while let Some((n, depth)) = stack.pop() {
            rows.push((n, depth));
            if self.expanded.contains(&n) {
                let kids: Vec<u32> = t.children(n).collect();
                for &c in kids.iter().rev() {
                    stack.push((c, depth + 1));
                }
            }
        }
        self.rows = rows;
        self.rows_gen = Some(self.version);
    }

    fn spawn_derived(&mut self, ctx: &egui::Context) {
        let (tx, rx) = channel();
        let tree = self.tree.clone();
        let version = self.version;
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("derived".into())
            .spawn(move || {
                let exts = tree.ext_stats(tree.root);
                let mut colors = vec![u8::MAX; tree.exts.len()];
                // 前 12 个（有扩展名的）类型各占一种颜色。
                for (k, &(e, _, _)) in exts.iter().filter(|e| e.0 != 0).take(12).enumerate() {
                    colors[e as usize] = k as u8;
                }
                // 元文件（$MFT 等）删不掉，不放进"最大的文件"。
                let largest: Vec<u32> = tree
                    .largest_files(tree.root, 320)
                    .into_iter()
                    .filter(|&n| !tree.node(n).has(flags::META))
                    .take(300)
                    .collect();
                drop(tree);
                let _ = tx.send(Derived {
                    version,
                    exts,
                    colors: Arc::new(colors),
                    largest,
                });
                ctx.request_repaint();
            })
            .expect("spawn derived");
        self.derived_rx = Some(rx);
        self.derived_requested = Some(version);
    }

    /// 清理页打开时调用：数据变过就重新计算清理建议。
    pub fn want_suggestions(&mut self, ctx: &egui::Context) {
        if self.suggest_requested == Some(self.version) || self.suggest_rx.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let tree = self.tree.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("suggest".into())
            .spawn(move || {
                let _ = tx.send(cleanup::compute(tree));
                ctx.request_repaint();
            })
            .expect("spawn suggest");
        self.suggest_rx = Some(rx);
        self.suggest_requested = Some(self.version);
    }

    pub fn suggestions_loading(&self) -> bool {
        self.suggest_rx.is_some()
    }

    fn poll_search(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.search.rx
            && let Ok((q, version, results)) = rx.try_recv()
        {
            self.search.rx = None;
            self.search.running = None;
            self.search.done = Some((q, version));
            self.search.results = results;
        }
        let q = self.search.query.trim().to_string();
        if q.is_empty() {
            self.search.results.clear();
            self.search.done = None;
            return;
        }
        let want = (q.clone(), self.version);
        if self.search.done.as_ref() == Some(&want) || self.search.running.is_some() {
            return;
        }
        let (tx, rx) = channel();
        let tree = self.tree.clone();
        let ctx = ctx.clone();
        let version = self.version;
        std::thread::Builder::new()
            .name("search".into())
            .spawn(move || {
                let r = tree.search(&q, 2000);
                drop(tree);
                let _ = tx.send((q, version, r));
                ctx.request_repaint();
            })
            .expect("spawn search");
        self.search.rx = Some(rx);
        self.search.running = Some(want);
    }

    pub fn mutate(&mut self, m: Mutation) {
        self.pending.push(m);
    }

    /// 只有没有后台线程持有树时才能原地修改；否则留到下一帧。
    fn apply_pending(&mut self, ctx: &egui::Context) {
        if self.pending.is_empty() || self.basis_rx.is_some() {
            return;
        }
        let Some(tree) = Arc::get_mut(&mut self.tree) else {
            ctx.request_repaint_after(Duration::from_millis(30));
            return;
        };
        for m in self.pending.drain(..) {
            match m {
                Mutation::Remove(nodes) => {
                    tree.remove_many(&nodes);
                }
                Mutation::Graft(n, sub) => {
                    if !tree.node(n).has(flags::DELETED) {
                        tree.graft(n, &sub);
                    }
                }
            }
        }
        // 被删掉的节点移出选中项；键盘光标、缩放根如果被删了，退回到还在的祖先。
        // 光标只移动、不选中祖先，免得连按两次 Delete 删掉上级文件夹。
        let alive = |t: &Tree, n: u32| !t.node(n).has(flags::DELETED);
        let tree = &*self.tree;
        self.selection.retain(|n| alive(tree, n));
        if let Some(f) = self.selection.focus()
            && !alive(tree, f)
        {
            self.selection
                .set_focus(tree.ancestors(f).find(|&a| alive(tree, a)));
        }
        if !alive(tree, self.zoom) {
            self.zoom = tree
                .ancestors(self.zoom)
                .find(|&a| alive(tree, a))
                .unwrap_or(tree.root);
        }
        self.expanded.retain(|&n| alive(tree, n));
        self.bump();
    }

    /// 切换大小口径（后台复制一份重新排序，完成后整体替换）。
    pub fn set_basis(&mut self, by_alloc: bool, ctx: &egui::Context) {
        if self.tree.by_alloc == by_alloc && self.basis_rx.is_none() {
            return;
        }
        let (tx, rx) = channel();
        let tree = self.tree.clone();
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("basis".into())
            .spawn(move || {
                let mut t = (*tree).clone();
                drop(tree);
                t.set_basis(by_alloc);
                let _ = tx.send(t);
                ctx.request_repaint();
            })
            .expect("spawn basis");
        self.basis_rx = Some(rx);
    }

    pub fn basis_busy(&self) -> bool {
        self.basis_rx.is_some()
    }

    /// 选中并展开到这个节点，列表滚动到它。
    pub fn reveal(&mut self, n: u32) {
        let ancestors: Vec<u32> = self.tree.ancestors(n).skip(1).collect();
        for a in ancestors {
            self.expanded.insert(a);
        }
        self.selection.set_single(n);
        self.rebuild_rows();
        self.scroll_to = self.rows.iter().position(|r| r.0 == n);
    }

    pub fn toggle(&mut self, n: u32) {
        if !self.expanded.remove(&n) {
            self.expanded.insert(n);
        }
        self.invalidate_rows();
    }

    /// 矩形树图放大到 `n`（文件则取它所在的目录）。
    pub fn zoom_to(&mut self, n: u32) {
        let t = &self.tree;
        let dir = if t.node(n).is_dir() {
            n
        } else {
            t.node(n).parent
        };
        if dir != NONE {
            self.zoom = dir;
        }
    }

    /// 矩形树图退回上一级。
    pub fn zoom_out(&mut self) {
        let p = self.tree.node(self.zoom).parent;
        if p != NONE {
            self.zoom = p;
        }
    }

    /// 键盘导航：在当前可见的行之间移动。`extend`（Shift）时选中锚点到这一行的范围，否则只选这一行。
    pub fn select_row(&mut self, row: usize, extend: bool) {
        let Some(&(n, _)) = self.rows.get(row) else {
            return;
        };
        if extend {
            let rows = &self.rows;
            self.selection
                .extend_to(|i| rows[i].0, rows.len(), row, false);
        } else {
            self.selection.set_single(n);
        }
        self.scroll_to = Some(row);
    }

    /// 键盘光标所在的行。
    pub fn focus_row(&self) -> Option<usize> {
        let f = self.selection.focus()?;
        self.rows.iter().position(|r| r.0 == f)
    }

    /// 树形列表当前的顺序：搜索时是搜索结果，否则是展开后可见的行。
    pub fn visible(&self) -> Vec<u32> {
        if self.search.active() {
            self.search.results.clone()
        } else {
            self.rows.iter().map(|r| r.0).collect()
        }
    }

    /// 清理页"最大的文件"列表（去掉已删除的）。
    pub fn largest(&self) -> Vec<u32> {
        let t = &self.tree;
        self.derived
            .as_ref()
            .map(|d| {
                d.largest
                    .iter()
                    .copied()
                    .filter(|&n| !t.node(n).has(flags::DELETED))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Ctrl+A（分析页）：选中当前文件夹（面包屑 / 矩形树图的根）里的所有项。
    /// 这个文件夹在树里展开着时选它下面所有可见的行（与资源管理器"全选当前视图"一致，
    /// 展开的子文件夹里的项也算，删除前会规范化）；没展开或看不到时选它的直接子项。
    /// 搜索时选中全部搜索结果。
    pub fn select_all_in_view(&mut self) {
        if self.search.active() {
            let results = self.search.results.clone();
            self.selection.select_all(results);
            return;
        }
        let zoom = self.zoom;
        let pos = self.rows.iter().position(|r| r.0 == zoom);
        match pos {
            Some(i) if self.expanded.contains(&zoom) => {
                let depth = self.rows[i].1;
                let under = self.rows[i + 1..]
                    .iter()
                    .take_while(|r| r.1 > depth)
                    .map(|r| r.0);
                self.selection.select_all(under);
            }
            _ => {
                let t = self.tree.clone();
                self.selection.select_all(t.children(zoom));
            }
        }
    }

    /// 选中项的数量、规范化后的合计大小、能删除的项数。只在选中项或数据变了时重算。
    pub fn summary(&mut self) -> SelSummary {
        let key = (self.selection.generation(), self.version);
        if let Some((k, s)) = self.summary
            && k == key
        {
            return s;
        }
        let t = &self.tree;
        let total = self
            .selection
            .normalized(t)
            .iter()
            .map(|&n| t.value(n))
            .sum();
        let deletable = self.selection.deletion_set(t, crate::deletable).0.len();
        let s = SelSummary {
            count: self.selection.len(),
            total,
            deletable,
        };
        self.summary = Some((key, s));
        s
    }
}
