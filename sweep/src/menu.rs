//! 节点的右键菜单。按节点下标生成，不依赖列表的行号：列表重新排序、删除后行号会变，下标不会。
//!
//! 右键一个已选中的项、且选中了不止一项时，菜单作用于整个选择：标题显示项数和合计大小，
//! 复制和删除针对全部选中项；只对单项有意义的操作（在资源管理器中显示、属性）灰掉，
//! 打开文件夹、放大、重新扫描这类取决于类型的操作不出现。

use eframe::egui::{self, Ui};
use sweep_scan::{NONE, t};

use crate::doc::{Doc, SelSummary};
use crate::icons_gen::Icon;
use crate::{Action, deletable, fmt_bytes, n_items, widgets};

/// 一个菜单项：点了就记下操作。返回是否点了。
fn item(
    ui: &mut Ui,
    glyph: Icon,
    label: &str,
    danger: bool,
    a: Action,
    out: &mut Vec<Action>,
) -> bool {
    let clicked = widgets::menu_item(ui, glyph, label, danger).clicked();
    if clicked {
        out.push(a);
    }
    clicked
}

/// 删除的两个菜单项（回收站、永久删除），作用于当前选择。
fn delete_items(ui: &mut Ui, a: &mut Vec<Action>) -> bool {
    ui.separator();
    let mut acted = item(
        ui,
        Icon::Trash,
        t!("移到回收站", "Move to Recycle Bin", "ごみ箱に移動"),
        false,
        Action::DeleteSelection { permanent: false },
        a,
    );
    acted |= item(
        ui,
        Icon::TrashX,
        t!("永久删除…", "Delete permanently…", "完全に削除…"),
        true,
        Action::DeleteSelection { permanent: true },
        a,
    );
    acted
}

/// 返回 (产生的操作, 菜单是否继续显示)。
pub fn node_menu(
    ctx: &egui::Context,
    doc: &Doc,
    n: u32,
    pos: egui::Pos2,
    summary: SelSummary,
    deleting: bool,
) -> (Vec<Action>, bool) {
    if summary.count > 1 && doc.selection.contains(n) {
        multi_menu(ctx, doc, pos, summary, deleting)
    } else {
        single_menu(ctx, doc, n, pos, deleting)
    }
}

fn multi_menu(
    ctx: &egui::Context,
    doc: &Doc,
    pos: egui::Pos2,
    summary: SelSummary,
    deleting: bool,
) -> (Vec<Action>, bool) {
    let mut actions = Vec::new();
    let tree = &doc.tree;
    let open = widgets::floating_menu(ctx, "node-menu", pos, 230.0, |ui| {
        let a = &mut actions;
        let mut acted = false;
        let (items, size) = (n_items(summary.count as u64), fmt_bytes(summary.total));
        widgets::menu_heading(ui, &format!("{items} · {size}"), 0.0);
        let why = t!(
            "只能用于单个项目",
            "Only available for a single item",
            "1 つの項目を選択しているときだけ使用できます"
        );
        widgets::menu_item_disabled(
            ui,
            Icon::ExternalLink,
            t!(
                "在资源管理器中显示",
                "Show in File Explorer",
                "エクスプローラーで表示"
            ),
            why,
        );
        widgets::submenu(
            ui,
            Icon::Copy,
            t!("复制", "Copy", "コピー"),
            180.0,
            |ui| {
                let sel = doc.selection.items();
                for (label, full) in [
                    (t!("完整路径", "Full paths", "フル パス"), true),
                    (t!("名称", "Names", "名前"), false),
                ] {
                    if widgets::menu_plain(ui, label).clicked() {
                        let lines: Vec<String> = sel
                            .iter()
                            .map(|&k| {
                                if full {
                                    tree.path(k)
                                } else {
                                    tree.name(k).to_string()
                                }
                            })
                            .collect();
                        a.push(Action::Copy(lines.join("\r\n")));
                        acted = true;
                    }
                }
            },
        );
        widgets::menu_item_disabled(ui, Icon::Info, t!("属性", "Properties", "プロパティ"), why);
        if summary.deletable > 0 && !deleting {
            acted |= delete_items(ui, a);
        }
        acted
    });
    (actions, open)
}

fn single_menu(
    ctx: &egui::Context,
    doc: &Doc,
    n: u32,
    pos: egui::Pos2,
    deleting: bool,
) -> (Vec<Action>, bool) {
    let mut actions = Vec::new();
    let tree = &doc.tree;
    let node = *tree.node(n);
    let path = tree.path(n);
    let parent_path = if node.parent == NONE {
        path.clone()
    } else {
        tree.path(node.parent)
    };
    let open = widgets::floating_menu(ctx, "node-menu", pos, 230.0, |ui| {
        let a = &mut actions;
        let mut acted = false;
        widgets::menu_heading(ui, crate::short_name(tree, n), 0.0);
        acted |= item(
            ui,
            Icon::ExternalLink,
            t!(
                "在资源管理器中显示",
                "Show in File Explorer",
                "エクスプローラーで表示"
            ),
            false,
            Action::Reveal(path.clone()),
            a,
        );
        if node.is_dir() {
            acted |= item(
                ui,
                Icon::FolderOpen,
                t!("打开文件夹", "Open folder", "フォルダーを開く"),
                false,
                Action::Open(path.clone()),
                a,
            );
        }
        widgets::submenu(
            ui,
            Icon::Copy,
            t!("复制", "Copy", "コピー"),
            180.0,
            |ui| {
                for (label, text) in [
                    (t!("完整路径", "Full path", "フル パス"), path.clone()),
                    (t!("名称", "Name", "名前"), tree.name(n).to_string()),
                    (
                        t!("所在文件夹", "Parent folder", "親フォルダー"),
                        parent_path.clone(),
                    ),
                ] {
                    if widgets::menu_plain(ui, label).clicked() {
                        a.push(Action::Copy(text));
                        acted = true;
                    }
                }
            },
        );
        acted |= item(
            ui,
            Icon::Info,
            t!("属性", "Properties", "プロパティ"),
            false,
            Action::Properties(path.clone()),
            a,
        );
        ui.separator();
        if node.is_dir() {
            acted |= item(
                ui,
                Icon::Target,
                t!("在矩形树图中放大", "Zoom in treemap", "ツリーマップで拡大"),
                false,
                Action::ZoomTo(n),
                a,
            );
            acted |= item(
                ui,
                Icon::Refresh,
                t!(
                    "重新扫描此文件夹",
                    "Rescan this folder",
                    "このフォルダーを再スキャン"
                ),
                false,
                Action::Rescan(n),
                a,
            );
        } else {
            acted |= item(
                ui,
                Icon::Target,
                t!(
                    "放大到所在文件夹",
                    "Zoom to parent folder",
                    "親フォルダーまで拡大"
                ),
                false,
                Action::ZoomTo(n),
                a,
            );
        }
        // 右键时这一项已成为唯一的选中项，删除选择就是删除它。
        if deletable(tree, n) && !deleting {
            acted |= delete_items(ui, a);
        }
        acted
    });
    (actions, open)
}
