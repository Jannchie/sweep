//! 把变更套用到从缓存载入的树上：重新列出有变动的目录，与树上的子节点逐个比对。
//!
//! 树此时还没有汇总（目录的大小等都是 0），只改文件自身的数据和目录结构，最后统一调用
//! [`Tree::finish`]。分两步做：
//! 1. 列出每个目录，更新原有的子项，摘除已经不在的；记录号变了的（删除后重建、被别的文件替换）
//!    也摘除，留到第 2 步当作新项加回来；
//! 2. 加入新出现的项，新目录整棵遍历。
//!
//! 先摘后加，是因为在同一卷内移动的文件记录号不变：要是先在新位置加入，旧位置还没摘除，
//! 它会被误认成硬链接。

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};

use crate::tree::{NONE, Tree, flags};
use crate::walk::{self, Raw, record_of};
use crate::{Progress, tf};

/// 待加入的新项：父目录、目录项。
struct Pending {
    parent: u32,
    path: String,
    raw: Raw,
}

/// 套用变更。`dirs` 是有变动的目录（完整路径，可以包含扫描目标以外的）。返回实际重新列出的目录数。
pub(crate) fn apply(
    tree: &mut Tree,
    ids: &mut Vec<u64>,
    mut dirs: Vec<String>,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<usize, String> {
    // 浅的先处理：父目录被摘除后，其中的子目录自然找不到，不必再列。
    dirs.sort_by_key(|d| d.matches('\\').count());
    dirs.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    progress.total.store(dirs.len() as u64, Ordering::Relaxed);

    let mut pending = Vec::new();
    let mut listed = 0;
    for (k, dir) in dirs.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(crate::cancelled());
        }
        progress.done.store(k as u64, Ordering::Relaxed);
        if k % 64 == 0 {
            progress.set_current(dir.clone());
        }
        let Some(node) = tree.find(dir) else { continue };
        if !tree.node(node).is_dir() {
            continue;
        }
        let Ok(entries) = walk::list(&walk::long_path(dir)) else {
            continue; // 打不开（刚被删除等），由上级目录的变更处理。
        };
        listed += 1;
        refresh(tree, ids, node, dir, entries, &mut pending);
    }

    // 记录号 → 已有的文件，用来认出新出现的硬链接。只在有新文件时才建。
    let mut known: Option<HashSet<u64>> = None;
    for p in pending {
        if cancel.load(Ordering::Relaxed) {
            return Err(crate::cancelled());
        }
        let known = known.get_or_insert_with(|| file_records(tree, ids));
        add(tree, ids, p, known, progress, cancel)?;
    }
    progress.done.store(dirs.len() as u64, Ordering::Relaxed);
    Ok(listed)
}

/// 第 1 步：比对一个目录。
fn refresh(
    tree: &mut Tree,
    ids: &mut [u64],
    node: u32,
    path: &str,
    entries: Vec<Raw>,
    pending: &mut Vec<Pending>,
) {
    let mut existing: HashMap<String, u32> = HashMap::new();
    for c in tree.children(node) {
        // 元文件（$MFT 等）不会出现在目录列表里，原样保留。
        if !tree.node(c).has(flags::META) {
            existing.insert(tree.name(c).to_lowercase(), c);
        }
    }
    let mut gone = Vec::new();
    for raw in entries {
        let key = raw.name.to_lowercase();
        let Some(c) = existing.remove(&key) else {
            pending.push(Pending {
                parent: node,
                path: path.to_string(),
                raw,
            });
            continue;
        };
        let old = *tree.node(c);
        let same = old.is_dir() == raw.is_dir() && ids[c as usize] == record_of(raw.id);
        if !same {
            gone.push(c);
            pending.push(Pending {
                parent: node,
                path: path.to_string(),
                raw,
            });
            continue;
        }
        let dup = old.has(flags::HARDLINK);
        let mut e = raw.entry(dup);
        // 列表里的值变了：可能真的变了，也可能是目录索引里的旧值，打开文件确认。
        if !old.is_dir() && !dup && (e.size, e.alloc, e.mtime) != (old.size, old.alloc, old.mtime) {
            exact(&mut e, path, &raw.name);
        }
        let n = &mut tree.nodes[c as usize];
        if !old.is_dir() {
            n.size = e.size;
            n.alloc = e.alloc;
        }
        n.mtime = e.mtime;
        n.flags = e.flags | (old.flags & flags::DENIED);
        if tree.name(c) != raw.name {
            tree.set_name(c, &raw.name); // 只改了大小写
        }
    }
    gone.extend(existing.into_values());
    detach(tree, node, &gone);
}

/// 把 `gone` 从 `parent` 的子链表上摘下，整棵子树打上 DELETED。
fn detach(tree: &mut Tree, parent: u32, gone: &[u32]) {
    if gone.is_empty() {
        return;
    }
    let gone: HashSet<u32> = gone.iter().copied().collect();
    for &g in &gone {
        for k in tree.bfs(g) {
            tree.nodes[k as usize].flags |= flags::DELETED;
        }
    }
    let mut prev = NONE;
    let mut c = tree.nodes[parent as usize].first_child;
    while c != NONE {
        let next = tree.nodes[c as usize].next_sibling;
        if gone.contains(&c) {
            tree.nodes[c as usize].next_sibling = NONE;
            tree.nodes[c as usize].parent = NONE;
        } else {
            if prev == NONE {
                tree.nodes[parent as usize].first_child = c;
            } else {
                tree.nodes[prev as usize].next_sibling = c;
            }
            prev = c;
        }
        c = next;
    }
    if prev == NONE {
        tree.nodes[parent as usize].first_child = NONE;
    } else {
        tree.nodes[prev as usize].next_sibling = NONE;
    }
}

/// 树上所有文件的记录号。
fn file_records(tree: &Tree, ids: &[u64]) -> HashSet<u64> {
    tree.bfs(tree.root)
        .into_iter()
        .filter(|&i| !tree.node(i).is_dir())
        .map(|i| ids[i as usize])
        .filter(|&r| r != 0)
        .collect()
}

/// 第 2 步：加入一个新项；新目录整棵遍历后接上。
fn add(
    tree: &mut Tree,
    ids: &mut Vec<u64>,
    p: Pending,
    known: &mut HashSet<u64>,
    progress: &Progress,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let rec = record_of(p.raw.id);
    let dup = !p.raw.is_dir() && rec != 0 && !known.insert(rec);
    let mut e = p.raw.entry(dup);
    if !p.raw.is_dir() && !dup {
        exact(&mut e, &p.path, &p.raw.name);
    }
    let node = tree.push(p.parent, &p.raw.name, e);
    ids.push(rec);
    if !p.raw.enter() {
        return Ok(());
    }
    let path = join(&p.path, &p.raw.name);
    let quiet = Progress::default();
    let (sub, sub_ids) = match walk::scan_raw(&path, &quiet, cancel) {
        Ok(s) => s,
        Err(_) if cancel.load(Ordering::Relaxed) => return Err(crate::cancelled()),
        Err(_) => return Ok(()), // 刚建好又被删掉之类，留一个空目录。
    };
    progress.set_current(tf!(
        "已遍历新文件夹 {path}",
        "Walked new folder {path}",
        "新しいフォルダー {path} を走査しました"
    ));
    tree.nodes[node as usize].flags |= sub.node(sub.root).flags & flags::DENIED;
    let mut map = vec![NONE; sub.nodes.len()];
    map[sub.root as usize] = node;
    for i in sub.bfs(sub.root).into_iter().skip(1) {
        let s = sub.node(i);
        let r = sub_ids[i as usize];
        // 子树里的文件也可能是树上已有文件的硬链接。
        let dup = !s.is_dir() && (s.has(flags::HARDLINK) || (r != 0 && !known.insert(r)));
        let mut e = crate::tree::Entry {
            size: s.size,
            alloc: s.alloc,
            mtime: s.mtime,
            flags: s.flags,
        };
        if dup {
            e.size = 0;
            e.alloc = 0;
            e.flags |= flags::HARDLINK;
        }
        map[i as usize] = tree.push(map[s.parent as usize], sub.name(i), e);
        ids.push(r);
    }
    Ok(())
}

/// 用打开文件取到的当前值替换目录列表里的值。
fn exact(e: &mut crate::tree::Entry, dir: &str, name: &str) {
    if let Some((size, alloc, mtime)) = walk::file_info(&walk::join(&walk::long_path(dir), name)) {
        e.size = size;
        e.alloc = alloc;
        e.mtime = mtime;
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.ends_with('\\') {
        format!("{dir}{name}")
    } else {
        format!("{dir}\\{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn write(p: &Path, len: usize) {
        fs::write(p, vec![b'x'; len]).unwrap();
    }

    /// 在临时目录里造一棵树，扫描后改动文件，再按"有变动的目录"套用，结果应与重新扫描一致。
    #[test]
    fn matches_a_fresh_scan() {
        let base = std::env::temp_dir().join(format!("sweep-test-incr-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        for d in [
            "keep",
            "grow",
            "moved/inner",
            "dst",
            "gone/deep",
            "recreate",
        ] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        write(&base.join("keep/a.bin"), 100);
        write(&base.join("grow/log.txt"), 10);
        write(&base.join("grow/old.txt"), 20);
        write(&base.join("moved/inner/m.bin"), 300);
        write(&base.join("moved/f.bin"), 40);
        write(&base.join("gone/deep/x.bin"), 50);
        write(&base.join("recreate/r.bin"), 60);
        write(&base.join("Case.txt"), 70);
        let root = base.to_str().unwrap().to_string();

        let p = Progress::default();
        let no = AtomicBool::new(false);
        let (mut tree, mut ids) = walk::scan_raw(&root, &p, &no).unwrap();

        // 变更：文件变大、删除、新建，目录移动、删除、删除后同名重建，改大小写，新建目录。
        write(&base.join("grow/log.txt"), 5000);
        fs::remove_file(base.join("grow/old.txt")).unwrap();
        write(&base.join("grow/new.txt"), 7);
        fs::rename(base.join("moved"), base.join("dst/moved")).unwrap();
        fs::rename(base.join("dst/moved/f.bin"), base.join("keep/f.bin")).unwrap();
        fs::remove_dir_all(base.join("gone")).unwrap();
        fs::remove_dir_all(base.join("recreate")).unwrap();
        fs::create_dir_all(base.join("recreate/sub")).unwrap();
        write(&base.join("recreate/sub/s.bin"), 80);
        fs::rename(base.join("Case.txt"), base.join("case.txt")).unwrap();
        fs::create_dir_all(base.join("fresh/a/b")).unwrap();
        write(&base.join("fresh/a/b/n.bin"), 90);
        // 新文件是已有文件的硬链接：不能重复计算大小。
        fs::hard_link(base.join("keep/a.bin"), base.join("grow/link.bin")).unwrap();

        // 日志里会出现的目录：每个被改动项的父目录（移动的两端都有）。
        let dirs: Vec<String> = [
            "grow",
            "",
            "dst",
            "dst/moved",
            "keep",
            "recreate",
            "recreate/sub",
            "fresh",
            "fresh/a",
            "fresh/a/b",
            "gone",
            "gone/deep",
        ]
        .iter()
        .map(|d| {
            join(&root, &d.replace('/', "\\"))
                .trim_end_matches('\\')
                .to_string()
        })
        .collect();
        apply(&mut tree, &mut ids, dirs, &p, &no).unwrap();
        tree.finish();
        assert_eq!(ids.len(), tree.len());

        let fresh = walk::scan(&root, false, &p, &no).unwrap();
        // 硬链接的大小算在哪个名字上取决于遍历顺序，keep 与 grow 两个目录不比大小。
        let linked = |path: &str| path.contains(r"\keep") || path.contains(r"\grow");
        let summary = |t: &Tree| {
            let mut v: Vec<(String, Option<u64>, u32, u32)> = t
                .bfs(t.root)
                .into_iter()
                .map(|i| {
                    let (n, path) = (t.node(i), t.path(i));
                    let size = (!linked(&path) && i != t.root).then_some(n.size);
                    (path, size, n.files, n.dirs)
                })
                .collect();
            v.sort();
            v
        };
        assert_eq!(summary(&tree), summary(&fresh));
        assert_eq!(tree.node(tree.root).size, fresh.node(fresh.root).size);
        let links = |t: &Tree| {
            let a = t.find(&format!(r"{root}\keep\a.bin")).unwrap();
            let b = t.find(&format!(r"{root}\grow\link.bin")).unwrap();
            t.node(a).has(flags::HARDLINK) != t.node(b).has(flags::HARDLINK)
        };
        assert!(links(&tree), "两个名字里恰好一个是硬链接");
        assert!(tree.find(&format!(r"{root}\case.txt")).is_some());
        assert_eq!(
            tree.node(tree.root).size,
            100 + 5000 + 7 + 300 + 40 + 80 + 70 + 90
        );
        let _ = fs::remove_dir_all(&base);
    }
}
