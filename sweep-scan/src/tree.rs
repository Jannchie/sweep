//! 扫描结果：一个 `Vec<Node>` 组成的树，节点之间用 u32 下标相连，名字放在同一个字符串池里。
//!
//! 每个文件约 56 字节的节点 + 名字本身（UTF-8），几百万个文件也只要几百 MB 以内。
//! 节点下标在整个生命周期里保持不变：删除只是把节点从父节点上摘下并打上 [`flags::DELETED`]，
//! 重新扫描子树时新节点追加在末尾。UI 可以放心地用下标记住选中项、展开状态。

use std::collections::HashMap;

/// 表示"没有"的下标（无父节点、无子节点、无兄弟）。
pub const NONE: u32 = u32::MAX;

pub mod flags {
    pub const DIR: u8 = 1;
    /// 硬链接的另一个名字。大小只算在第一个名字上，这里大小为 0。
    pub const HARDLINK: u8 = 2;
    /// 重解析点（目录联接、符号链接、云文件占位符等）。目录型的不会进入。
    pub const REPARSE: u8 = 4;
    /// NTFS 元文件（$MFT、$LogFile、$Extend 下的 $UsnJrnl 等），只读展示，不能删除。
    pub const META: u8 = 8;
    pub const HIDDEN: u8 = 16;
    pub const SYSTEM: u8 = 32;
    /// 没有权限列出内容，大小不完整。
    pub const DENIED: u8 = 64;
    /// 已从树上摘除（删除或被重新扫描替换）。
    pub const DELETED: u8 = 128;
}

#[derive(Clone, Copy, Debug)]
pub struct Node {
    /// 文件的逻辑大小；目录为子树合计。
    pub size: u64,
    /// 磁盘上实际占用（簇对齐、压缩、稀疏后的分配大小）；目录为子树合计。
    pub alloc: u64,
    /// 最后修改时间（FILETIME，100ns 自 1601 年起）；目录取子树里最新的。
    pub mtime: u64,
    pub parent: u32,
    pub first_child: u32,
    pub next_sibling: u32,
    name_off: u32,
    /// 子树里的文件数（文件自身为 1）。
    pub files: u32,
    /// 子树里的文件夹数（不含自身）。
    pub dirs: u32,
    name_len: u16,
    /// 扩展名在 [`Tree::exts`] 里的下标，0 表示没有扩展名（目录也是 0）。
    pub ext: u16,
    pub flags: u8,
}

impl Node {
    pub fn is_dir(&self) -> bool {
        self.flags & flags::DIR != 0
    }

    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

#[derive(Clone, Debug)]
pub struct Tree {
    pub nodes: Vec<Node>,
    names: String,
    pub root: u32,
    /// 扩展名表（小写，不含点），下标 0 是空串。由 [`Tree::finish`] 生成。
    pub exts: Vec<String>,
    /// 排序、查询按实际占用（true）还是逻辑大小（false）。见 [`Tree::set_basis`]。
    pub by_alloc: bool,
}

/// 新建节点时的参数。
#[derive(Clone, Copy, Debug, Default)]
pub struct Entry {
    pub size: u64,
    pub alloc: u64,
    pub mtime: u64,
    pub flags: u8,
}

impl Tree {
    /// 只有一个根目录的空树。`root_name` 是根的完整路径（如 `C:\` 或 `D:\data`），
    /// 拼接路径时直接作为前缀。
    pub fn new(root_name: &str) -> Self {
        let mut t = Self {
            nodes: Vec::new(),
            names: String::new(),
            root: 0,
            exts: vec![String::new()],
            by_alloc: false,
        };
        t.push(
            NONE,
            root_name,
            Entry {
                flags: flags::DIR,
                ..Default::default()
            },
        );
        t
    }

    pub fn with_capacity(root_name: &str, nodes: usize, name_bytes: usize) -> Self {
        let mut t = Self::new(root_name);
        t.nodes.reserve(nodes);
        t.names.reserve(name_bytes);
        t
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// 节点和名字池占用的内存（字节），不含 Vec 的空余容量。
    pub fn memory(&self) -> usize {
        self.nodes.len() * size_of::<Node>() + self.names.len()
    }

    /// 追加一个节点，挂到 `parent` 的子节点链表头部（`parent` 为 [`NONE`] 时不挂）。
    /// 大小等统计要等 [`Tree::finish`] 才会向上汇总。
    pub fn push(&mut self, parent: u32, name: &str, e: Entry) -> u32 {
        let index = self.nodes.len() as u32;
        let (name_off, name_len) = self.intern(name);
        let is_dir = e.flags & flags::DIR != 0;
        self.nodes.push(Node {
            size: e.size,
            alloc: e.alloc,
            mtime: e.mtime,
            parent: NONE,
            first_child: NONE,
            next_sibling: NONE,
            name_off,
            files: u32::from(!is_dir),
            dirs: 0,
            name_len,
            ext: 0,
            flags: e.flags,
        });
        if parent != NONE {
            self.link(parent, index);
        }
        index
    }

    /// 把 `child` 挂到 `parent` 下（链表头部）。`child` 当前不能挂在别处。
    pub fn link(&mut self, parent: u32, child: u32) {
        let head = self.nodes[parent as usize].first_child;
        let c = &mut self.nodes[child as usize];
        c.parent = parent;
        c.next_sibling = head;
        self.nodes[parent as usize].first_child = child;
    }

    fn intern(&mut self, name: &str) -> (u32, u16) {
        // 单个文件名在 NTFS 上最长 255 个 UTF-16 单元，UTF-8 最多 765 字节，u16 足够；
        // 根路径可能更长，超出的部分截断到字符边界。
        let mut end = name.len().min(u16::MAX as usize);
        while !name.is_char_boundary(end) {
            end -= 1;
        }
        let off = self.names.len() as u32;
        self.names.push_str(&name[..end]);
        (off, end as u16)
    }

    pub fn set_name(&mut self, i: u32, name: &str) {
        let (off, len) = self.intern(name);
        let n = &mut self.nodes[i as usize];
        n.name_off = off;
        n.name_len = len;
    }

    pub fn name(&self, i: u32) -> &str {
        let n = &self.nodes[i as usize];
        &self.names[n.name_off as usize..n.name_off as usize + n.name_len as usize]
    }

    pub fn node(&self, i: u32) -> &Node {
        &self.nodes[i as usize]
    }

    /// 按当前口径（[`Tree::by_alloc`]）的大小。
    pub fn value(&self, i: u32) -> u64 {
        let n = &self.nodes[i as usize];
        if self.by_alloc { n.alloc } else { n.size }
    }

    /// 切换大小口径并按新口径重新排序所有目录。几百万个节点约需一秒，应在后台线程调用。
    pub fn set_basis(&mut self, by_alloc: bool) {
        if self.by_alloc == by_alloc {
            return;
        }
        self.by_alloc = by_alloc;
        let mut buf = Vec::new();
        for i in self.bfs(self.root) {
            if self.nodes[i as usize].is_dir() {
                self.sort_children(i, &mut buf);
            }
        }
    }

    pub fn ext_name(&self, i: u32) -> &str {
        &self.exts[self.nodes[i as usize].ext as usize]
    }

    /// 子节点迭代器（[`Tree::finish`] 之后按大小从大到小）。
    pub fn children(&self, i: u32) -> Children<'_> {
        Children {
            tree: self,
            next: self.nodes[i as usize].first_child,
        }
    }

    /// 从 `i` 往上直到根（含 `i` 与根）。
    pub fn ancestors(&self, i: u32) -> impl Iterator<Item = u32> + '_ {
        std::iter::successors(Some(i), move |&n| {
            let p = self.nodes[n as usize].parent;
            (p != NONE).then_some(p)
        })
    }

    pub fn depth(&self, i: u32) -> usize {
        self.ancestors(i).count() - 1
    }

    /// 完整路径。根节点的名字就是扫描目标的路径。
    pub fn path(&self, i: u32) -> String {
        let chain: Vec<u32> = self.ancestors(i).collect();
        let mut s = String::new();
        for &n in chain.iter().rev() {
            if !s.is_empty() && !s.ends_with('\\') {
                s.push('\\');
            }
            s.push_str(self.name(n));
        }
        s
    }

    /// 从根开始按路径逐级查找（不区分大小写）。`path` 必须以根路径开头。
    pub fn find(&self, path: &str) -> Option<u32> {
        let root = self.name(self.root).trim_end_matches('\\');
        let rest = strip_prefix_ci(path.trim_end_matches('\\'), root)?;
        let mut cur = self.root;
        for part in rest.split('\\').filter(|s| !s.is_empty()) {
            cur = self
                .children(cur)
                .find(|&c| self.name(c).eq_ignore_ascii_case(part) || self.name(c) == part)?;
        }
        Some(cur)
    }

    /// 是否在 `ancestor` 的子树里（含自身）。
    pub fn is_within(&self, i: u32, ancestor: u32) -> bool {
        self.ancestors(i).any(|a| a == ancestor)
    }

    /// 扫描结束后调用一次：自底向上汇总大小、数量、修改时间，各目录的子节点按大小从大到小排序，
    /// 生成扩展名表。
    pub fn finish(&mut self) {
        let order = self.bfs(self.root);
        // 逆 BFS 序就是"子节点先于父节点"，一趟就能汇总完。
        for &i in order.iter().rev() {
            let n = self.nodes[i as usize];
            if n.parent == NONE || n.has(flags::DELETED) {
                continue;
            }
            let p = &mut self.nodes[n.parent as usize];
            p.size += n.size;
            p.alloc += n.alloc;
            p.files += n.files;
            p.dirs += n.dirs + u32::from(n.is_dir());
            p.mtime = p.mtime.max(n.mtime);
        }
        let mut buf = Vec::new();
        for &i in &order {
            if self.nodes[i as usize].is_dir() {
                self.sort_children(i, &mut buf);
            }
        }
        self.build_exts(&order);
    }

    /// 从 `start` 开始的广度优先序（含自身）。
    pub fn bfs(&self, start: u32) -> Vec<u32> {
        let mut order = vec![start];
        let mut k = 0;
        while k < order.len() {
            let mut c = self.nodes[order[k] as usize].first_child;
            while c != NONE {
                order.push(c);
                c = self.nodes[c as usize].next_sibling;
            }
            k += 1;
        }
        order
    }

    fn sort_children(&mut self, i: u32, buf: &mut Vec<u32>) {
        buf.clear();
        let mut c = self.nodes[i as usize].first_child;
        while c != NONE {
            buf.push(c);
            c = self.nodes[c as usize].next_sibling;
        }
        if buf.len() < 2 {
            return;
        }
        // 大小相同时按名字排，结果稳定，重新扫描后顺序不会乱跳。
        buf.sort_unstable_by(|&a, &b| {
            self.value(b)
                .cmp(&self.value(a))
                .then_with(|| self.name(a).cmp(self.name(b)))
        });
        self.nodes[i as usize].first_child = buf[0];
        for w in buf.windows(2) {
            self.nodes[w[0] as usize].next_sibling = w[1];
        }
        self.nodes[*buf.last().unwrap() as usize].next_sibling = NONE;
    }

    fn build_exts(&mut self, order: &[u32]) {
        let mut map: HashMap<String, u16> = self
            .exts
            .iter()
            .enumerate()
            .map(|(i, e)| (e.clone(), i as u16))
            .collect();
        let mut lower = String::new();
        for &i in order {
            let n = &self.nodes[i as usize];
            if n.is_dir() || n.ext != 0 {
                continue;
            }
            let Some(ext) = extension(self.name(i)) else {
                continue;
            };
            lower.clear();
            lower.extend(ext.chars().flat_map(char::to_lowercase));
            let id = match map.get(lower.as_str()) {
                Some(&id) => id,
                None if self.exts.len() < u16::MAX as usize => {
                    let id = self.exts.len() as u16;
                    self.exts.push(lower.clone());
                    map.insert(lower.clone(), id);
                    id
                }
                // 扩展名种类超过 65535 个时，其余的都算作"无扩展名"。
                None => 0,
            };
            self.nodes[i as usize].ext = id;
        }
    }

    /// 把 `i`（及其子树）从树上摘除，并从所有祖先的合计里减掉。返回被摘除的大小。
    pub fn remove(&mut self, i: u32) -> u64 {
        self.remove_many(&[i])
    }

    /// 一次摘除多个节点（及其子树）。同一目录下删掉十万个子项时，每个父目录的子链表只重建一次、
    /// 每个受影响的祖先只重新排序一次，而不是逐个摘除时的平方级开销。
    /// 已摘除的、根节点、以及祖先也在列表里的节点会被跳过（不会重复扣减）。返回摘除的大小合计。
    pub fn remove_many(&mut self, ids: &[u32]) -> u64 {
        let mut removed = 0;
        // 子链表里有被摘除项的父目录，以及大小变了、需要重新排序的祖先。
        let mut parents = Vec::new();
        let mut resort = Vec::new();
        for &i in ids {
            let n = self.nodes[i as usize];
            if n.parent == NONE || n.has(flags::DELETED) {
                continue;
            }
            let files = n.files;
            let dirs = n.dirs + u32::from(n.is_dir());
            let mut a = n.parent;
            while a != NONE {
                let p = &mut self.nodes[a as usize];
                p.size = p.size.saturating_sub(n.size);
                p.alloc = p.alloc.saturating_sub(n.alloc);
                p.files = p.files.saturating_sub(files);
                p.dirs = p.dirs.saturating_sub(dirs);
                resort.push(a);
                a = p.parent;
            }
            for k in self.bfs(i) {
                self.nodes[k as usize].flags |= flags::DELETED;
            }
            parents.push(n.parent);
            removed += n.size;
        }
        parents.sort_unstable();
        parents.dedup();
        for &p in &parents {
            if self.nodes[p as usize].has(flags::DELETED) {
                continue;
            }
            // 重建子链表：去掉打了 DELETED 的子节点，顺序不变。
            let mut prev = NONE;
            let mut c = self.nodes[p as usize].first_child;
            while c != NONE {
                let next = self.nodes[c as usize].next_sibling;
                if self.nodes[c as usize].has(flags::DELETED) {
                    self.nodes[c as usize].next_sibling = NONE;
                } else {
                    if prev == NONE {
                        self.nodes[p as usize].first_child = c;
                    } else {
                        self.nodes[prev as usize].next_sibling = c;
                    }
                    prev = c;
                }
                c = next;
            }
            if prev == NONE {
                self.nodes[p as usize].first_child = NONE;
            } else {
                self.nodes[prev as usize].next_sibling = NONE;
            }
        }
        // 大小变了，每一层的兄弟顺序都可能变；每个祖先只排一次。
        resort.sort_unstable();
        resort.dedup();
        let mut buf = Vec::new();
        for a in resort {
            if !self.nodes[a as usize].has(flags::DELETED) {
                self.sort_children(a, &mut buf);
            }
        }
        removed
    }

    /// 把子树 `n` 的统计加到（或减出）从 `from` 开始的每个祖先上，并重新排序受影响的兄弟链。
    fn adjust_ancestors(&mut self, from: u32, n: &Node, add: bool) {
        let files = n.files;
        let dirs = n.dirs + u32::from(n.is_dir());
        let chain: Vec<u32> = self.ancestors(from).collect();
        for &a in &chain {
            let a = &mut self.nodes[a as usize];
            if add {
                a.size += n.size;
                a.alloc += n.alloc;
                a.files += files;
                a.dirs += dirs;
                a.mtime = a.mtime.max(n.mtime);
            } else {
                a.size = a.size.saturating_sub(n.size);
                a.alloc = a.alloc.saturating_sub(n.alloc);
                a.files = a.files.saturating_sub(files);
                a.dirs = a.dirs.saturating_sub(dirs);
            }
        }
        // 大小变了，每一层的兄弟顺序都可能变。
        let mut buf = Vec::new();
        for &a in &chain {
            self.sort_children(a, &mut buf);
        }
    }

    /// 用重新扫描得到的 `sub`（根对应 `target`）替换 `target` 的内容。旧子节点打上 DELETED，
    /// 新节点追加到末尾，`target` 自身的下标不变。
    pub fn graft(&mut self, target: u32, sub: &Tree) {
        // 先把旧内容整体减掉。
        let old = self.nodes[target as usize];
        let mut c = old.first_child;
        while c != NONE {
            let next = self.nodes[c as usize].next_sibling;
            for k in self.bfs(c) {
                self.nodes[k as usize].flags |= flags::DELETED;
            }
            c = next;
        }
        if old.parent != NONE {
            self.adjust_ancestors(old.parent, &old, false);
        }

        // 复制新节点：sub 的下标 → 本树的下标。
        let mut map = vec![NONE; sub.nodes.len()];
        map[sub.root as usize] = target;
        let mut ext_map: HashMap<u16, u16> = HashMap::new();
        for i in sub.bfs(sub.root).into_iter().skip(1) {
            let s = sub.nodes[i as usize];
            let ext = if s.ext == 0 {
                0
            } else {
                *ext_map.entry(s.ext).or_insert_with(|| {
                    let name = &sub.exts[s.ext as usize];
                    match self.exts.iter().position(|e| e == name) {
                        Some(p) => p as u16,
                        None if self.exts.len() < u16::MAX as usize => {
                            self.exts.push(name.clone());
                            (self.exts.len() - 1) as u16
                        }
                        None => 0,
                    }
                })
            };
            let (name_off, name_len) = self.intern(sub.name(i));
            let k = self.nodes.len() as u32;
            self.nodes.push(Node {
                parent: NONE,
                first_child: NONE,
                next_sibling: NONE,
                name_off,
                name_len,
                ext,
                ..s
            });
            map[i as usize] = k;
        }
        // 按原顺序重新串起兄弟链：逆序头插，得到与 sub 相同的顺序。
        self.nodes[target as usize].first_child = NONE;
        for i in sub.bfs(sub.root).into_iter().skip(1).rev() {
            let parent = map[sub.nodes[i as usize].parent as usize];
            self.link(parent, map[i as usize]);
        }
        // sub 可能是按另一种口径排的序。
        if sub.by_alloc != self.by_alloc {
            let mut buf = Vec::new();
            for i in self.bfs(target) {
                if self.nodes[i as usize].is_dir() {
                    self.sort_children(i, &mut buf);
                }
            }
        }
        let r = sub.nodes[sub.root as usize];
        let t = &mut self.nodes[target as usize];
        t.size = r.size;
        t.alloc = r.alloc;
        t.files = r.files;
        t.dirs = r.dirs;
        t.mtime = r.mtime;
        t.flags = (t.flags & !flags::DENIED) | (r.flags & flags::DENIED);
        let new = *t;
        if new.parent != NONE {
            self.adjust_ancestors(new.parent, &new, true);
        }
    }

    /// 子树里按大小排前 `n` 的文件（大的在前）。
    pub fn largest_files(&self, root: u32, n: usize) -> Vec<u32> {
        use std::cmp::Reverse;
        use std::collections::BinaryHeap;
        let mut heap: BinaryHeap<Reverse<(u64, u32)>> = BinaryHeap::with_capacity(n + 1);
        let mut stack = vec![root];
        while let Some(i) = stack.pop() {
            let node = &self.nodes[i as usize];
            if node.is_dir() {
                // 子节点按大小降序，比堆里最小的还小的目录可以整个跳过。
                if heap.len() == n && heap.peek().is_some_and(|m| self.value(i) <= m.0.0) {
                    continue;
                }
                let mut c = node.first_child;
                while c != NONE {
                    stack.push(c);
                    c = self.nodes[c as usize].next_sibling;
                }
            } else if self.value(i) > 0 {
                heap.push(Reverse((self.value(i), i)));
                if heap.len() > n {
                    heap.pop();
                }
            }
        }
        let mut v: Vec<(u64, u32)> = heap.into_iter().map(|r| r.0).collect();
        v.sort_unstable_by(|a, b| b.cmp(a));
        v.into_iter().map(|(_, i)| i).collect()
    }

    /// 子树里按扩展名汇总：(扩展名下标, 大小, 文件数)，大的在前。
    pub fn ext_stats(&self, root: u32) -> Vec<(u16, u64, u64)> {
        let mut acc = vec![(0u64, 0u64); self.exts.len()];
        let mut stack = vec![root];
        while let Some(i) = stack.pop() {
            let node = &self.nodes[i as usize];
            if node.is_dir() {
                let mut c = node.first_child;
                while c != NONE {
                    stack.push(c);
                    c = self.nodes[c as usize].next_sibling;
                }
            } else {
                let a = &mut acc[node.ext as usize];
                a.0 += self.value(i);
                a.1 += 1;
            }
        }
        let mut v: Vec<(u16, u64, u64)> = acc
            .into_iter()
            .enumerate()
            .filter(|(_, (_, n))| *n > 0)
            .map(|(e, (s, n))| (e as u16, s, n))
            .collect();
        v.sort_unstable_by_key(|e| std::cmp::Reverse(e.1));
        v
    }

    /// 名字包含 `query` 的节点（不区分大小写），按大小从大到小，最多 `limit` 个。
    pub fn search(&self, query: &str, limit: usize) -> Vec<u32> {
        let q = query.to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }
        let ascii = q.is_ascii();
        let mut hits: Vec<u32> = (0..self.nodes.len() as u32)
            .filter(|&i| {
                let n = &self.nodes[i as usize];
                if n.has(flags::DELETED) || i == self.root {
                    return false;
                }
                let name = self.name(i);
                if ascii {
                    contains_ascii_ci(name.as_bytes(), q.as_bytes())
                } else {
                    name.to_lowercase().contains(&q)
                }
            })
            .collect();
        hits.sort_unstable_by_key(|&i| std::cmp::Reverse(self.value(i)));
        hits.truncate(limit);
        hits
    }
}

pub struct Children<'a> {
    tree: &'a Tree,
    next: u32,
}

impl Iterator for Children<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        if self.next == NONE {
            return None;
        }
        let i = self.next;
        self.next = self.tree.nodes[i as usize].next_sibling;
        Some(i)
    }
}

/// 文件名的扩展名（不含点）。以点开头的隐藏文件（`.gitignore`）和过长的"扩展名"不算。
pub fn extension(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    let ext = &name[dot + 1..];
    (dot > 0 && !ext.is_empty() && ext.len() <= 16 && !ext.contains(' ')).then_some(ext)
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

/// `hay` 是否包含 `needle`（`needle` 已是小写 ASCII）。
fn contains_ascii_ci(hay: &[u8], needle: &[u8]) -> bool {
    if needle.len() > hay.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| {
        w.iter()
            .zip(needle)
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(size: u64) -> Entry {
        Entry {
            size,
            alloc: size.div_ceil(4096) * 4096,
            mtime: size,
            flags: 0,
        }
    }

    fn dir() -> Entry {
        Entry {
            flags: flags::DIR,
            ..Default::default()
        }
    }

    /// C:\ ─ a\ (x.txt 10, y.LOG 30) ─ b\ (c\ (z.bin 100)) ─ top.txt 5
    fn sample() -> (Tree, [u32; 8]) {
        let mut t = Tree::new(r"C:\");
        let a = t.push(t.root, "a", dir());
        let x = t.push(a, "x.txt", file(10));
        let y = t.push(a, "y.LOG", file(30));
        let b = t.push(t.root, "b", dir());
        let c = t.push(b, "c", dir());
        let z = t.push(c, "z.bin", file(100));
        let top = t.push(t.root, "top.txt", file(5));
        t.finish();
        let root = t.root;
        (t, [root, a, x, y, b, c, z, top])
    }

    #[test]
    fn aggregates_and_sorts() {
        let (t, [root, a, _x, y, b, c, z, top]) = sample();
        assert_eq!(t.node(root).size, 145);
        assert_eq!(t.node(root).files, 4);
        assert_eq!(t.node(root).dirs, 3);
        assert_eq!(t.node(a).size, 40);
        assert_eq!(t.node(root).mtime, 100);
        assert_eq!(t.children(root).collect::<Vec<_>>(), vec![b, a, top]);
        assert_eq!(t.children(a).next(), Some(y));
        assert_eq!(t.path(z), r"C:\b\c\z.bin");
        assert_eq!(t.find(r"c:\B\c\Z.BIN"), Some(z));
        assert_eq!(t.find(r"C:\"), Some(root));
        assert_eq!(t.ext_name(y), "log");
        assert_eq!(t.depth(z), 3);
        assert!(t.is_within(z, b) && !t.is_within(z, a));
        let _ = c;
    }

    #[test]
    fn remove_updates_ancestors_and_order() {
        let (mut t, [root, a, x, y, b, c, z, _]) = sample();
        assert_eq!(t.remove(z), 100);
        assert_eq!(t.node(c).size, 0);
        assert_eq!(t.node(b).files, 0);
        assert_eq!(t.node(root).size, 45);
        assert_eq!(t.node(root).files, 3);
        // b 变成 0 之后排到了 a 后面。
        assert_eq!(t.children(root).next(), Some(a));
        assert!(t.node(z).has(flags::DELETED));
        assert_eq!(t.remove(z), 0, "重复删除不再扣减");
        t.remove(a);
        assert_eq!(t.node(root).size, 5);
        assert_eq!(t.node(root).dirs, 2);
        assert!(t.node(x).has(flags::DELETED) && t.node(y).has(flags::DELETED));
        assert!(t.search("x.txt", 10).is_empty());
    }

    #[test]
    fn remove_many_skips_nested_and_repeats() {
        let (mut t, [root, a, x, y, b, c, z, top]) = sample();
        // a 和它里面的 x 都在列表里：只扣一次；根与重复项被跳过。
        assert_eq!(t.remove_many(&[x, a, root, a, z]), 140);
        assert_eq!(t.node(root).size, 5);
        assert_eq!(t.node(root).files, 1);
        assert_eq!(t.node(root).dirs, 2);
        assert_eq!(t.node(b).size, 0);
        assert!(t.node(y).has(flags::DELETED) && t.node(z).has(flags::DELETED));
        assert!(!t.node(c).has(flags::DELETED));
        assert_eq!(t.children(root).collect::<Vec<_>>(), vec![top, b]);
        assert_eq!(t.children(c).count(), 0);
    }

    #[test]
    fn remove_many_wide_folder() {
        let mut t = Tree::new(r"C:\");
        let d = t.push(t.root, "wide", dir());
        let ids: Vec<u32> = (0..20_000u64)
            .map(|k| t.push(d, &format!("f{k}"), file(k + 1)))
            .collect();
        t.finish();
        let total = t.node(d).size;
        // 删掉偶数大小的一半。
        let gone: Vec<u32> = ids
            .iter()
            .copied()
            .filter(|&i| t.node(i).size.is_multiple_of(2))
            .collect();
        let freed: u64 = gone.iter().map(|&i| t.node(i).size).sum();
        assert_eq!(t.remove_many(&gone), freed);
        assert_eq!(t.node(d).size, total - freed);
        assert_eq!(t.node(t.root).files, 10_000);
        let kids: Vec<u32> = t.children(d).collect();
        assert_eq!(kids.len(), 10_000);
        assert!(kids.windows(2).all(|w| t.value(w[0]) >= t.value(w[1])));
        assert!(kids.iter().all(|&k| !t.node(k).size.is_multiple_of(2)));
    }

    #[test]
    fn graft_replaces_subtree() {
        let (mut t, [root, a, x, _y, b, ..]) = sample();
        let mut sub = Tree::new(r"C:\a");
        let n = sub.push(sub.root, "new", dir());
        sub.push(n, "big.iso", file(1000));
        sub.finish();
        t.graft(a, &sub);
        assert_eq!(t.node(a).size, 1000);
        assert_eq!(t.node(root).size, 1105);
        assert_eq!(t.node(root).files, 3);
        assert_eq!(t.children(root).next(), Some(a));
        assert!(t.node(x).has(flags::DELETED));
        let big = t.find(r"C:\a\new\big.iso").unwrap();
        assert_eq!(t.ext_name(big), "iso");
        assert_eq!(t.node(b).size, 100);
    }

    #[test]
    fn queries() {
        let (t, [root, _a, x, y, _b, _c, z, top]) = sample();
        assert_eq!(t.largest_files(root, 2), vec![z, y]);
        assert_eq!(t.largest_files(root, 10), vec![z, y, x, top]);
        let stats = t.ext_stats(root);
        assert_eq!(t.exts[stats[0].0 as usize], "bin");
        assert_eq!(
            stats
                .iter()
                .find(|s| t.exts[s.0 as usize] == "txt")
                .unwrap()
                .2,
            2
        );
        assert_eq!(t.search("TXT", 10), vec![x, top]);
        assert_eq!(extension(".gitignore"), None);
        assert_eq!(extension("a.tar.gz"), Some("gz"));
        assert_eq!(extension("noext"), None);
    }

    #[test]
    fn switches_basis() {
        let mut t = Tree::new(r"C:\");
        let a = t.push(
            t.root,
            "sparse.bin",
            Entry {
                size: 1000,
                alloc: 0,
                ..file(0)
            },
        );
        let b = t.push(
            t.root,
            "real.bin",
            Entry {
                size: 10,
                alloc: 4096,
                ..file(0)
            },
        );
        t.finish();
        assert_eq!(t.children(t.root).next(), Some(a));
        t.set_basis(true);
        assert_eq!(t.children(t.root).next(), Some(b));
        assert_eq!(t.value(t.root), 4096);
        assert_eq!(t.largest_files(t.root, 1), vec![b]);
    }

    #[test]
    fn node_is_compact() {
        assert!(size_of::<Node>() <= 56);
    }
}
