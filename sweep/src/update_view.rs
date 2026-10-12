//! 更新相关的界面：标题栏里的新版本提示、更新对话框、设置页的检查状态。状态来自 [`crate::update`]。

use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, Rect, Response, RichText,
    Sense, Ui, pos2, vec2,
};
use sweep_scan::{t, tf};

use crate::dialogs::{self, Kind, PRIMARY_FILL, PRIMARY_FILL_HOVER};
use crate::icons_gen::Icon;
use crate::theme::{Palette, semibold};
use crate::update::{Check, Error, Install, Method, Release};
use crate::widgets;

/// 更新对话框里用户的选择。
pub enum Choice {
    Install,
    Later,
    Skip,
    /// exe 所在目录不可写，改为在浏览器中打开发布页。
    OpenPage,
}

/// 标题栏里的"新版本"小标签。
pub fn title_pill(ui: &mut Ui, version: &str) -> Response {
    let label = tf!(
        "新版本 v{version}",
        "Update v{version}",
        "新しいバージョン v{version}"
    );
    let font = FontId::new(12.0, semibold());
    let galley = ui.painter().layout_no_wrap(label, font, Color32::WHITE);
    let (rect, resp) = ui.allocate_exact_size(vec2(galley.size().x + 38.0, 26.0), Sense::click());
    let fill = if resp.hovered() {
        PRIMARY_FILL_HOVER
    } else {
        PRIMARY_FILL
    };
    ui.painter().rect_filled(rect, CornerRadius::same(13), fill);
    widgets::icon(
        ui,
        Icon::Download,
        pos2(rect.left() + 17.0, rect.center().y),
        13.0,
        Color32::WHITE,
    );
    ui.painter().galley(
        pos2(rect.left() + 28.0, rect.center().y - galley.size().y / 2.0),
        galley,
        Color32::WHITE,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
        .on_hover_text(t!(
            "查看更新内容并更新",
            "See what's new and update",
            "更新内容を確認して更新する"
        ))
}

/// 更新对话框。安装进行中时不能关闭。
pub fn dialog(
    ctx: &egui::Context,
    app: &str,
    current: &str,
    release: &Release,
    install: &Install,
) -> Option<Choice> {
    let p = Palette::of(ctx);
    let busy = install.busy();
    let (choice, closed) = dialogs::modal_with(ctx, "update", 460.0, |ui| {
        let mut choice = None;
        ui.spacing_mut().item_spacing.y = 6.0;
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::hover());
            widgets::icon(ui, Icon::Download, r.center(), 20.0, p.accent);
            ui.label(
                RichText::new(tf!(
                    "{app} {} 可用",
                    "{app} {} is available",
                    "{app} {} を利用できます",
                    release.version
                ))
                .size(16.0)
                .family(semibold())
                .color(p.text),
            );
        });
        ui.label(
            RichText::new(tf!(
                "当前版本 {current}",
                "You have version {current}",
                "現在のバージョン {current}"
            ))
            .size(12.5)
            .color(p.muted),
        );
        ui.add_space(6.0);
        ui.label(
            RichText::new(t!("更新内容", "What's new", "更新内容"))
                .size(12.5)
                .family(semibold())
                .color(p.text),
        );
        Frame::NONE
            .fill(p.bg)
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        let notes = if release.notes.is_empty() {
                            t!(
                                "此版本没有发行说明。",
                                "This release has no notes.",
                                "このリリースにはリリース ノートがありません。"
                            )
                        } else {
                            release.notes.as_str()
                        };
                        ui.label(RichText::new(notes).size(12.5).color(p.muted));
                    });
            });

        ui.add_space(4.0);
        match install {
            Install::Downloading { done, total } => {
                progress(ui, *done as f32 / (*total).max(1) as f32);
                ui.label(
                    RichText::new(tf!(
                        "正在下载… {} / {}",
                        "Downloading… {} of {}",
                        "ダウンロードしています… {} / {}",
                        mb(*done),
                        mb(*total)
                    ))
                    .size(12.0)
                    .color(p.muted),
                );
            }
            Install::Verifying => {
                progress(ui, 1.0);
                ui.label(
                    RichText::new(t!(
                        "正在校验下载的文件…",
                        "Verifying the download…",
                        "ダウンロードしたファイルを検証しています…"
                    ))
                    .size(12.0)
                    .color(p.muted),
                );
            }
            Install::Restarting => {
                progress(ui, 1.0);
                ui.label(
                    RichText::new(tf!(
                        "正在重新启动 {app}…",
                        "Restarting {app}…",
                        "{app} を再起動しています…"
                    ))
                    .size(12.0)
                    .color(p.muted),
                );
            }
            Install::Failed(e) => {
                ui.label(
                    RichText::new(tf!(
                        "更新失败：{}。当前版本未受影响。",
                        "Couldn't update: {}. Your current version was left unchanged.",
                        "更新できませんでした：{}。現在のバージョンはそのまま残っています。",
                        error_text(e)
                    ))
                    .size(12.5)
                    .color(p.danger),
                );
            }
            Install::Idle if release.method == Method::Installer { elevate: true } => {
                ui.label(
                    RichText::new(t!(
                        "将运行安装程序完成更新，Windows 会请求管理员权限。",
                        "The installer will run to finish the update. Windows will ask for administrator permission.",
                        "インストーラーを実行して更新を完了します。Windows から管理者権限の確認が求められます。"
                    ))
                    .size(12.0)
                    .color(p.muted),
                );
            }
            Install::Idle if release.method == Method::Manual => {
                ui.label(
                    RichText::new(tf!(
                        "{app} 所在的文件夹不可写（例如位于 Program Files），无法自动更新。请从下载页面获取新版本。",
                        "{app}'s folder isn't writable (for example, it's under Program Files), so it can't update itself. Get the new version from the download page.",
                        "{app} のフォルダーに書き込めない (Program Files の下にあるなど) ため、自動更新できません。ダウンロード ページから新しいバージョンを入手してください。"
                    ))
                    .size(12.0)
                    .color(p.muted),
                );
            }
            Install::Idle => {}
        }

        ui.add_space(10.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            if release.method != Method::Manual {
                if dialogs::button(
                    ui,
                    t!("更新并重启", "Update and restart", "更新して再起動"),
                    Kind::Primary,
                    !busy,
                )
                .clicked()
                {
                    choice = Some(Choice::Install);
                }
            } else if dialogs::button(
                ui,
                t!(
                    "打开下载页面",
                    "Open download page",
                    "ダウンロード ページを開く"
                ),
                Kind::Primary,
                true,
            )
            .clicked()
            {
                choice = Some(Choice::OpenPage);
            }
            if dialogs::button(ui, t!("稍后", "Later", "後で"), Kind::Secondary, !busy).clicked()
            {
                choice = Some(Choice::Later);
            }
            ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                if dialogs::button(
                    ui,
                    t!(
                        "跳过此版本",
                        "Skip this version",
                        "このバージョンをスキップ"
                    ),
                    Kind::Secondary,
                    !busy,
                )
                .clicked()
                {
                    choice = Some(Choice::Skip);
                }
            });
        });
        choice
    });
    if busy {
        return None;
    }
    choice.or(closed.then_some(Choice::Later))
}

fn progress(ui: &mut Ui, frac: f32) {
    let p = Palette::of(ui.ctx());
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 6.0), Sense::hover());
    ui.painter()
        .rect_filled(rect, CornerRadius::same(3), p.track);
    let fill = Rect::from_min_size(
        rect.min,
        vec2(rect.width() * frac.clamp(0.0, 1.0), rect.height()),
    );
    ui.painter()
        .rect_filled(fill, CornerRadius::same(3), p.accent);
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// 设置页"关于"里检查更新的状态文字及颜色；没有可说的时返回 None。
pub fn check_status(ui: &Ui, check: &Check) -> Option<(String, Color32)> {
    let p = Palette::of(ui.ctx());
    Some(match check {
        Check::Idle => return None,
        Check::Checking => (
            t!(
                "正在检查更新…",
                "Checking for updates…",
                "更新を確認しています…"
            )
            .into(),
            p.muted,
        ),
        Check::UpToDate => (
            t!("已是最新版本", "You're up to date", "最新バージョンです").into(),
            p.muted,
        ),
        Check::Available(r) => (
            tf!(
                "有新版本 v{}",
                "Version {} is available",
                "バージョン {} を利用できます",
                r.version
            ),
            p.text,
        ),
        Check::Failed(e) => (
            tf!(
                "检查更新失败：{}",
                "Couldn't check for updates: {}",
                "更新を確認できませんでした：{}",
                error_text(e)
            ),
            p.danger,
        ),
    })
}

pub fn error_text(e: &Error) -> String {
    match e {
        Error::Network(code) => tf!(
            "无法连接到 GitHub（错误 {code}）",
            "couldn't connect to GitHub (error {code})",
            "GitHub に接続できませんでした (エラー {code})"
        ),
        Error::Http(status) => tf!(
            "GitHub 返回了 HTTP {status}",
            "GitHub returned HTTP {status}",
            "GitHub から HTTP {status} が返されました"
        ),
        Error::RateLimited => t!(
            "GitHub 请求次数超出限制，请稍后再试",
            "GitHub's rate limit was reached; try again later",
            "GitHub のリクエスト数の上限に達しました。しばらくしてからお試しください"
        )
        .into(),
        Error::BadResponse => t!(
            "无法识别 GitHub 的响应",
            "GitHub sent an unexpected response",
            "GitHub からの応答を認識できませんでした"
        )
        .into(),
        Error::AssetMissing => t!(
            "该版本没有可下载的 exe",
            "this release has no exe to download",
            "このリリースにはダウンロードできる exe がありません"
        )
        .into(),
        Error::ChecksumMissing => t!(
            "找不到下载文件的校验和，已取消更新",
            "no checksum was published for the download, so the update was canceled",
            "ダウンロード ファイルのチェックサムが見つからないため、更新を中止しました"
        )
        .into(),
        Error::ChecksumMismatch => t!(
            "下载的文件未通过 SHA-256 校验，已取消更新",
            "the download failed SHA-256 verification, so the update was canceled",
            "ダウンロードしたファイルが SHA-256 の検証に失敗したため、更新を中止しました"
        )
        .into(),
        Error::NotWritable => t!(
            "程序所在的文件夹不可写",
            "the app's folder isn't writable",
            "アプリのフォルダーに書き込めません"
        )
        .into(),
        Error::Cancelled => t!(
            "未获得管理员权限",
            "administrator permission was not granted",
            "管理者権限が許可されませんでした"
        )
        .into(),
        Error::Io(msg) => msg.trim().trim_end_matches(['。', '.']).to_string(),
    }
}
