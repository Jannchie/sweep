//! 节点的右键菜单。按节点下标生成，不依赖列表的行号：列表重新排序、删除后行号会变，下标不会。

use eframe::egui::{self, Ui};
use sweep_scan::{NONE, Tree, t};

use crate::icons_gen::Icon;
use crate::{Action, deletable, widgets};

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

/// 返回 (产生的操作, 菜单是否继续显示)。
pub fn node_menu(
    ctx: &egui::Context,
    tree: &Tree,
    n: u32,
    pos: egui::Pos2,
    deleting: bool,
) -> (Vec<Action>, bool) {
    let mut actions = Vec::new();
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
        if deletable(tree, n) && !deleting {
            ui.separator();
            let recycle = Action::DeleteNodes {
                nodes: vec![n],
                permanent: false,
            };
            acted |= item(
                ui,
                Icon::Trash,
                t!("移到回收站", "Move to Recycle Bin", "ごみ箱に移動"),
                false,
                recycle,
                a,
            );
            let nuke = Action::DeleteNodes {
                nodes: vec![n],
                permanent: true,
            };
            acted |= item(
                ui,
                Icon::TrashX,
                t!("永久删除…", "Delete permanently…", "完全に削除…"),
                true,
                nuke,
                a,
            );
        }
        acted
    });
    (actions, open)
}
