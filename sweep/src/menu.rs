//! 节点的右键菜单。按节点下标生成，不依赖列表的行号：列表重新排序、删除后行号会变，下标不会。

use eframe::egui::{self, Ui};
use sweep_scan::{NONE, Tree};

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
            "在资源管理器中显示",
            false,
            Action::Reveal(path.clone()),
            a,
        );
        if node.is_dir() {
            acted |= item(
                ui,
                Icon::FolderOpen,
                "打开文件夹",
                false,
                Action::Open(path.clone()),
                a,
            );
        }
        widgets::submenu(ui, Icon::Copy, "复制", 160.0, |ui| {
            for (label, text) in [
                ("完整路径", path.clone()),
                ("名称", tree.name(n).to_string()),
                ("所在文件夹", parent_path.clone()),
            ] {
                if widgets::menu_plain(ui, label).clicked() {
                    a.push(Action::Copy(text));
                    acted = true;
                }
            }
        });
        acted |= item(
            ui,
            Icon::Info,
            "属性",
            false,
            Action::Properties(path.clone()),
            a,
        );
        ui.separator();
        if node.is_dir() {
            acted |= item(
                ui,
                Icon::Target,
                "在矩形树图中放大",
                false,
                Action::ZoomTo(n),
                a,
            );
            acted |= item(
                ui,
                Icon::Refresh,
                "重新扫描此文件夹",
                false,
                Action::Rescan(n),
                a,
            );
        } else {
            acted |= item(
                ui,
                Icon::Target,
                "放大到所在文件夹",
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
            acted |= item(ui, Icon::Trash, "移到回收站", false, recycle, a);
            let nuke = Action::DeleteNodes {
                nodes: vec![n],
                permanent: true,
            };
            acted |= item(ui, Icon::TrashX, "永久删除…", true, nuke, a);
        }
        acted
    });
    (actions, open)
}
