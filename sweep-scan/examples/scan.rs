//! 命令行基准与对照：
//!   cargo run --release -p sweep-scan --example scan -- C:\ --mft
//!   cargo run --release -p sweep-scan --example scan -- C:\ --walk
//!   cargo run --release -p sweep-scan --example scan -- C:\ --compare
//! 对照模式先读 MFT 再遍历目录，比较总大小、文件数和一批文件夹的大小。
//!   cargo run --release -p sweep-scan --example scan -- C:\ --incremental
//! 增量模式：完整扫描并保存缓存，再增量更新一次，与不用缓存的完整扫描比较。
//! 缓存写在 %LOCALAPPDATA%\Sweep\cache，可用环境变量 LOCALAPPDATA 指到别处；
//! 设置 SWEEP_TIME_CACHE=1 时另外计时缓存的读写。

use std::sync::atomic::AtomicBool;

use sweep_scan::{Engine, Progress, Tree, flags, system};

fn gib(b: u64) -> String {
    format!("{:.2} GiB", b as f64 / (1u64 << 30) as f64)
}

fn run(target: &str, engine: Engine) -> Tree {
    run_with(target, engine, false)
}

fn run_with(target: &str, engine: Engine, cache: bool) -> Tree {
    let progress = Progress::default();
    let out = sweep_scan::scan(
        target,
        engine,
        false,
        cache,
        &progress,
        &AtomicBool::new(false),
    )
    .unwrap_or_else(|e| panic!("扫描失败：{e}"));
    let t = std::sync::Arc::unwrap_or_clone(out.tree);
    let live = t.nodes.iter().filter(|n| !n.has(flags::DELETED)).count();
    let r = t.node(t.root);
    let (ws, peak, private) = system::memory();
    println!("== {:?} {target}", out.engine);
    if let Some((dirs, _)) = out.incremental {
        println!("   incremental: {dirs} folders relisted");
    }
    if let Some(e) = &out.mft_fallback {
        println!("   mft failed, walked instead: {e}");
    }
    println!(
        "   time {:.2}s  nodes {} (live {})  files {}  dirs {}  size {}  alloc {}",
        out.elapsed.as_secs_f64(),
        t.len(),
        live,
        r.files,
        r.dirs,
        gib(r.size),
        gib(r.alloc)
    );
    println!(
        "   tree {:.1} MiB ({:.1} B/node)  process: working set {:.0} MiB, peak {:.0} MiB, private {:.0} MiB",
        t.memory() as f64 / 1048576.0,
        t.memory() as f64 / t.len().max(1) as f64,
        ws as f64 / 1048576.0,
        peak as f64 / 1048576.0,
        private as f64 / 1048576.0
    );
    t
}

/// 去掉 NTFS 元文件后的 (大小, 文件数)。
fn user_totals(t: &Tree, i: u32) -> (u64, u64) {
    let n = t.node(i);
    let (mut size, mut files) = (n.size, n.files as u64);
    for c in t.children(i) {
        let cn = t.node(c);
        if cn.has(flags::META) {
            size -= cn.size;
            files -= cn.files as u64;
        }
    }
    (size, files)
}

fn count(t: &Tree, flag: u16) -> usize {
    t.nodes
        .iter()
        .filter(|n| n.has(flag) && !n.has(flags::DELETED))
        .count()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let target = args.first().map(String::as_str).unwrap_or(r"C:\");
    let mode = args.get(1).map(String::as_str).unwrap_or("--auto");
    match mode {
        "--mft" => {
            run(target, Engine::Mft);
        }
        "--walk" => {
            run(target, Engine::Walk);
        }
        "--compare" => compare(target),
        "--compare-dir" => compare_dir(target),
        "--incremental" => incremental(target),
        "--top" => {
            let t = run(target, Engine::Auto);
            top(&t);
        }
        _ => {
            run(target, Engine::Auto);
        }
    }
}

/// 对照一个文件夹：MFT 扫整个卷后取这个文件夹的子树，与遍历结果逐个节点比较。
fn compare_dir(dir: &str) {
    let dir = sweep_scan::normalize(dir);
    let volume = format!("{}\\", &dir[..2]);
    let m = run(&volume, Engine::Mft);
    let w = run(&dir, Engine::Walk);
    let Some(mroot) = m.find(&dir) else {
        println!("MFT 里找不到 {dir}");
        return;
    };
    let collect =
        |t: &Tree, root: u32| -> std::collections::BTreeMap<String, (u64, u64, u32, u16)> {
            t.bfs(root)
                .into_iter()
                .map(|i| {
                    let n = t.node(i);
                    let keep = flags::DIR | flags::HARDLINK | flags::REPARSE;
                    (
                        t.path(i).to_lowercase(),
                        (n.size, n.alloc, n.files, n.flags & keep),
                    )
                })
                .collect()
        };
    let (a, b) = (collect(&m, mroot), collect(&w, w.root));
    let mut bad = 0;
    for (path, x) in &a {
        match b.get(path) {
            Some(y) if x == y => {}
            other => {
                bad += 1;
                println!("   差异 {path}: mft {x:?} walk {other:?}");
            }
        }
    }
    for path in b.keys().filter(|p| !a.contains_key(*p)) {
        bad += 1;
        println!("   只在遍历里：{path}");
    }
    println!(
        "   {} 个节点（MFT {}，遍历 {}），{bad} 处不一致（(大小, 占用, 文件数, 标记)）",
        a.len().max(b.len()),
        a.len(),
        b.len()
    );
}

/// 完整扫描（保存缓存）→ 增量更新 → 不用缓存的完整扫描，比较后两者。
fn incremental(target: &str) {
    let target = sweep_scan::normalize(target);
    sweep_scan::cache::remove(&target);
    run_with(&target, Engine::Auto, true);
    if let Some(p) = sweep_scan::cache::file_for(&target) {
        let len = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        println!(
            "   cache {} ({:.1} MiB)",
            p.display(),
            len as f64 / 1048576.0
        );
    }
    if std::env::var("SWEEP_TIME_CACHE").is_ok() {
        let t = std::time::Instant::now();
        let (meta, tree, ids) = sweep_scan::cache::load(&target).unwrap();
        let load = t.elapsed();
        let t = std::time::Instant::now();
        sweep_scan::cache::save(&tree, &ids, &meta).unwrap();
        println!("   cache load {load:?}  save {:?}", t.elapsed());
    }
    let i = run_with(&target, Engine::Auto, true);
    let f = run_with(&target, Engine::Auto, false);
    let (a, b) = (i.node(i.root), f.node(f.root));
    println!(
        "
== 增量 vs 完整"
    );
    println!(
        "   size {} vs {} (diff {:+} B)  files {} vs {}  dirs {} vs {}",
        gib(a.size),
        gib(b.size),
        a.size as i64 - b.size as i64,
        a.files,
        b.files,
        a.dirs,
        b.dirs
    );
    // 沿着有差异的目录往下找，列出大小不同的文件。
    let mut stack = vec![f.root];
    let mut shown = 0;
    while let Some(d) = stack.pop() {
        for c in f.children(d) {
            let path = f.path(c);
            let y = f.node(c);
            let Some(x) = i.find(&path).map(|n| *i.node(n)) else {
                if shown < 30 {
                    println!("   只在完整扫描里：{path} {}", y.size);
                    shown += 1;
                }
                continue;
            };
            if x.size == y.size {
                continue;
            }
            if y.is_dir() {
                stack.push(c);
            } else if shown < 30 {
                println!(
                    "   差异 {path}: incremental {} / {} full {} / {}  mtime {} vs {}",
                    x.size, x.alloc, y.size, y.alloc, x.mtime, y.mtime
                );
                shown += 1;
            }
        }
    }
}

/// 根下最大的文件夹与整个卷最大的文件。
fn top(t: &Tree) {
    println!(
        "
   最大的第一层："
    );
    for c in t.children(t.root).take(15) {
        let n = t.node(c);
        println!("   {:>12} {:>12}  {}", gib(n.size), gib(n.alloc), t.path(c));
    }
    println!(
        "
   最大的文件："
    );
    for f in t.largest_files(t.root, 15) {
        let n = t.node(f);
        println!("   {:>12} {:>12}  {}", gib(n.size), gib(n.alloc), t.path(f));
    }
}

fn compare(target: &str) {
    let m = run(target, Engine::Mft);
    let w = run(target, Engine::Walk);
    let (ms, mf) = user_totals(&m, m.root);
    let ws = w.node(w.root).size;
    let wf = w.node(w.root).files as u64;
    println!("\n== 对照（MFT 去掉元文件）");
    println!(
        "   size  mft {}  walk {}  diff {:+.3} GiB ({:+.3}%)",
        gib(ms),
        gib(ws),
        (ms as f64 - ws as f64) / (1u64 << 30) as f64,
        (ms as f64 - ws as f64) / ws.max(1) as f64 * 100.0
    );
    println!(
        "   files mft {mf}  walk {wf}  diff {:+}",
        mf as i64 - wf as i64
    );
    let meta: u64 = m
        .children(m.root)
        .filter(|&c| m.node(c).has(flags::META))
        .map(|c| m.node(c).size)
        .sum();
    println!("   mft 元文件合计 {}", gib(meta));
    println!(
        "   hardlink names: mft {}  walk {};  walk 无权限目录 {};  重解析点 mft {} walk {}",
        count(&m, flags::HARDLINK),
        count(&w, flags::HARDLINK),
        count(&w, flags::DENIED),
        count(&m, flags::REPARSE),
        count(&w, flags::REPARSE)
    );

    let missing: Vec<String> = (0..m.len() as u32)
        .filter(|&i| m.node(i).has(flags::HARDLINK) && !m.node(i).has(flags::DELETED))
        .map(|i| m.path(i))
        .filter(|p| w.find(p).is_none())
        .take(8)
        .collect();
    println!("   mft 有、遍历没有的硬链接名（样例）：{missing:#?}");

    // 抽样：第一、二层里最大的若干个文件夹。
    let mut sample: Vec<u32> = m
        .children(m.root)
        .filter(|&c| m.node(c).is_dir() && !m.node(c).has(flags::META))
        .take(12)
        .collect();
    let second: Vec<u32> = sample
        .iter()
        .flat_map(|&d| m.children(d).filter(|&c| m.node(c).is_dir()).take(4))
        .collect();
    sample.extend(second);
    println!(
        "\n   {:<60} {:>14} {:>14} {:>10}",
        "folder", "mft", "walk", "diff MiB"
    );
    let (mut same, mut total) = (0, 0);
    for d in sample {
        let path = m.path(d);
        let ms = m.node(d).size;
        let (ws, mark) = match w.find(&path) {
            Some(i) => (
                w.node(i).size,
                if w.node(i).has(flags::DENIED) {
                    " denied"
                } else {
                    ""
                },
            ),
            None => (0, " missing"),
        };
        total += 1;
        if ms == ws {
            same += 1;
        }
        let shown: String = path.chars().take(60).collect();
        println!(
            "   {shown:<60} {ms:>14} {ws:>14} {:>+10.1}{mark}",
            (ms as f64 - ws as f64) / 1048576.0
        );
    }
    println!("   {same}/{total} 个文件夹大小完全一致");

    // 差异最大的第一层文件夹及其内部最大的差异来源。
    let mut diffs: Vec<(i64, String)> = Vec::new();
    collect_diffs(&m, &w, m.root, 0, &mut diffs);
    diffs.sort_by_key(|d| -d.0.abs());
    println!("\n   差异最大的位置（深度 ≤ 4）：");
    for (d, p) in diffs.iter().take(15) {
        println!("   {:>+12.1} MiB  {p}", *d as f64 / 1048576.0);
    }
}

/// 逐层下钻，记录"差异在此处产生"（子节点差异之和解释不了的部分）的目录。
fn collect_diffs(m: &Tree, w: &Tree, i: u32, depth: usize, out: &mut Vec<(i64, String)>) {
    let path = m.path(i);
    let Some(wi) = w.find(&path) else {
        out.push((m.node(i).size as i64, format!("{path}（遍历中缺失）")));
        return;
    };
    let d = m.node(i).size as i64 - w.node(wi).size as i64;
    if d == 0 {
        return;
    }
    if depth >= 4 {
        out.push((d, path));
        return;
    }
    let mut explained = 0i64;
    for c in m.children(i) {
        if !m.node(c).is_dir() || m.node(c).has(flags::META) {
            continue;
        }
        let cp = m.path(c);
        let cd = match w.find(&cp) {
            Some(wc) => m.node(c).size as i64 - w.node(wc).size as i64,
            None => m.node(c).size as i64,
        };
        if cd != 0 {
            explained += cd;
            collect_diffs(m, w, c, depth + 1, out);
        }
    }
    let meta: i64 = if i == m.root {
        m.children(i)
            .filter(|&c| m.node(c).has(flags::META))
            .map(|c| m.node(c).size as i64)
            .sum()
    } else {
        0
    };
    let rest = d - explained - meta;
    if rest != 0 {
        out.push((rest, format!("{path}（直接包含的文件）")));
    }
}
