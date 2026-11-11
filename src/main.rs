#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod background;
mod dictionary;
mod free_translate;
mod ipc;
mod language;
mod layout;
mod net;
mod screen_clip;
mod secret_store;
mod single_instance;
mod tray;
mod tts;
mod volcengine;
mod windows_ocr;
mod winrt_runtime;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use eframe::egui::{self, Color32, RichText, Stroke};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
        Arc, Weak,
    },
    thread,
    time::{Duration, Instant},
};

const CONFIG_FILE: &str = "config.json";
const OFFICIAL_WEBSITE: &str = "https://github.com/elaina-agi/ocr-translate";
const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
const DEFAULT_FONT_SIZE: u8 = 13;
const FONT_SIZE_RANGE: std::ops::RangeInclusive<u8> = 12..=22;
const TARGET_LANGUAGES: [&str; 9] = [
    "简体中文",
    "繁体中文",
    "英语",
    "日语",
    "韩语",
    "法语",
    "西班牙语",
    "俄语",
    "德语",
];

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TranslationMode {
    #[default]
    Tencent,
    Google,
    Volcengine,
    Vision,
}

impl TranslationMode {
    fn label(self) -> &'static str {
        match self {
            Self::Tencent => "腾讯交互翻译（免 Key）",
            Self::Google => "Google 翻译（免 Key，可能限流）",
            Self::Volcengine => "火山机器翻译（AK/SK）",
            Self::Vision => "自定义视觉模型",
        }
    }

    fn short_label(self) -> &'static str {
        match self {
            Self::Tencent => "腾讯翻译",
            Self::Google => "Google 翻译",
            Self::Volcengine => "火山翻译",
            Self::Vision => "视觉模型",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ThemeMode {
    Day,
    #[default]
    Night,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum CloseBehavior {
    #[default]
    #[serde(alias = "ask")]
    Tray,
    Exit,
}

impl CloseBehavior {
    fn label(self) -> &'static str {
        match self {
            Self::Tray => "隐藏到托盘",
            Self::Exit => "直接退出",
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct AppConfig {
    mode: TranslationMode,
    theme: ThemeMode,
    font_size: u8,
    close_behavior: CloseBehavior,
    ocr_language: String,
    api_url: String,
    api_key: String,
    model: String,
    target_language: String,
    /// Translate text already in the target language into `second_language` instead.
    two_way: bool,
    second_language: String,
    shortcut: String,
    proxy_mode: net::ProxyMode,
    proxy_url: String,
    volcengine_access_key: String,
    volcengine_secret_key: String,
    tts_engine: tts::Engine,
    /// Speaking speed for read-aloud; 1.0 is normal.
    tts_rate: f32,
    azure_speech_key: String,
    azure_speech_region: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            mode: TranslationMode::Tencent,
            theme: ThemeMode::Night,
            font_size: DEFAULT_FONT_SIZE,
            close_behavior: CloseBehavior::Tray,
            ocr_language: "en-US".into(),
            api_url: "https://api.openai.com/v1/chat/completions".into(),
            api_key: String::new(),
            model: "gpt-4o-mini".into(),
            target_language: "简体中文".into(),
            two_way: true,
            second_language: "英语".into(),
            shortcut: "Ctrl+Shift+T".into(),
            proxy_mode: net::ProxyMode::System,
            proxy_url: "http://127.0.0.1:7890".into(),
            volcengine_access_key: String::new(),
            volcengine_secret_key: String::new(),
            tts_engine: tts::Engine::Windows,
            tts_rate: 1.0,
            azure_speech_key: String::new(),
            azure_speech_region: "eastasia".into(),
        }
    }
}

impl AppConfig {
    fn proxy(&self) -> net::ProxySetting {
        net::ProxySetting {
            mode: self.proxy_mode,
            url: self.proxy_url.trim().to_owned(),
        }
    }

    fn secrets_mut(&mut self) -> [&mut String; 4] {
        [
            &mut self.api_key,
            &mut self.volcengine_access_key,
            &mut self.volcengine_secret_key,
            &mut self.azure_speech_key,
        ]
    }

    /// A copy for writing to disk, with every non-empty secret encrypted.
    fn protected(&self) -> Result<Self, String> {
        let mut stored = self.clone();
        for secret in stored.secrets_mut() {
            if !secret.is_empty() {
                *secret = secret_store::protect(secret)?;
            }
        }
        Ok(stored)
    }

    /// Decrypts secrets read from disk. Returns whether plaintext secrets saved by an older
    /// version were found, and whether any encrypted secret could not be decrypted.
    fn reveal_secrets(&mut self) -> (bool, bool) {
        let (mut plaintext, mut undecryptable) = (false, false);
        for secret in self.secrets_mut() {
            if secret_store::is_protected(secret) {
                match secret_store::unprotect(secret) {
                    Ok(plain) => *secret = plain,
                    Err(_) => {
                        secret.clear();
                        undecryptable = true;
                    }
                }
            } else if !secret.is_empty() {
                plaintext = true;
            }
        }
        (plaintext, undecryptable)
    }
}

const POPUP_WIDTH: f32 = 380.0;
const POPUP_MAX_HEIGHT: f32 = 420.0;
const POPUP_MARGIN: i8 = 14;

/// A floating result window shown next to a hotkey screen capture. Several can be on
/// screen at once: pinned popups survive the next capture, each holding its own result.
struct ResultPopup {
    /// Unique across the session; gives the viewport its identity.
    id: u64,
    task_id: Option<u64>,
    /// Where the selection ended and the work area of that monitor, in physical pixels.
    anchor: [i32; 2],
    work_area: [i32; 4],
    opened_at: Instant,
    pinned: bool,
    show_source: bool,
    copied: bool,
    /// Physical rectangle last requested for the window, to tell clicks inside from outside.
    placed: Option<egui::Rect>,
    /// The window starts hidden and appears once it has the geometry it was drawn for.
    shown: bool,
    width: f32,
    height: f32,
    /// Physical cursor and window geometry captured at the start of a drag.
    drag_origin: Option<([i32; 2], egui::Rect)>,
    /// Physical cursor and window geometry captured at the start of a resize.
    resize_origin: Option<([i32; 2], egui::Rect)>,
    /// The user dragged or resized this window, so stop placing it at the anchor.
    moved: bool,
    /// The user resized the window away from the content-fitted size.
    user_sized: bool,
    /// Image view (translation painted over the capture) or text view; decided per result.
    image_view: Option<bool>,
    /// The capture uploaded for the image view, keyed by the layout it came from.
    texture: Option<(Weak<layout::ImageLayout>, egui::TextureHandle)>,
    /// The result snapshot this popup displays; the main window keeps its own latest copy.
    result: TranslationResult,
    /// Waiting for a translation to fill `result`; drives the popup's spinner.
    pending: bool,
    /// Spawned before its outcome event arrived; adopted instead of doubled by that event.
    provisional: bool,
    /// Status line for this popup's own progress or error, independent of other popups.
    status: String,
    escape_was_down: bool,
    click_was_down: bool,
    /// Set during drawing when the popup should be gone after this frame.
    close_requested: bool,
}

/// What the user asked of a popup while it was drawn this frame.
#[derive(Default)]
struct PopupActions {
    close: bool,
    open_main_window: bool,
    toggle_speech: Option<tts::Part>,
}

/// Read-aloud state of a popup's two texts.
struct PopupSpeech {
    translation: tts::Status,
    source: tts::Status,
}

impl PopupSpeech {
    fn error(&self) -> Option<&str> {
        [&self.translation, &self.source]
            .into_iter()
            .find_map(|status| match status {
                tts::Status::Failed(error) => Some(error.as_str()),
                _ => None,
            })
    }
}

#[cfg(test)]
fn retain_pinned_popups(popups: &mut Vec<ResultPopup>) {
    popups.retain(|popup| popup.pinned);
}

fn retain_pinned_and_provisional_popups(popups: &mut Vec<ResultPopup>) {
    popups.retain(|popup| popup.pinned || popup.provisional);
}

fn discard_provisional_popups(popups: &mut Vec<ResultPopup>) {
    popups.retain(|popup| !popup.provisional);
}

fn restore_popup_result(popup: &mut ResultPopup, result: TranslationResult, status: &str) {
    popup.provisional = false;
    popup.pending = false;
    popup.result = result;
    popup.status = status.into();
}

impl ResultPopup {
    /// Keep a texture only while it is actually displayed, at the physical display size.
    fn prepare_texture(
        &mut self,
        ctx: &egui::Context,
        page: Option<&Arc<layout::ImageLayout>>,
        size: Option<egui::Vec2>,
    ) {
        let Some((page, size)) = page.zip(size).filter(|_| self.image_view == Some(true)) else {
            self.texture = None;
            return;
        };
        // A weak reference prevents a reused allocation address from matching an older image.
        let key = Arc::downgrade(page);
        let pixels = size * ctx.pixels_per_point();
        let limit = ctx.input(|input| input.max_texture_side).max(1);
        let scale = (limit as f32 / pixels.x.max(pixels.y)).min(1.0);
        let dimensions = [
            (pixels.x * scale).ceil().max(1.0) as usize,
            (pixels.y * scale).ceil().max(1.0) as usize,
        ];
        let dimensions = [
            dimensions[0].min(page.width as usize),
            dimensions[1].min(page.height as usize),
        ];
        if self
            .texture
            .as_ref()
            .is_some_and(|(loaded, texture)| loaded.ptr_eq(&key) && texture.size() == dimensions)
        {
            return;
        }
        // Drop the previous handle before allocating the replacement.
        self.texture = None;
        let image = if dimensions == [page.width as usize, page.height as usize] {
            egui::ColorImage::from_rgba_unmultiplied(dimensions, &page.rgba)
        } else {
            let original = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(
                page.width,
                page.height,
                page.rgba.as_slice(),
            )
            .expect("layout contains a complete RGBA image");
            let resized = image::imageops::resize(
                &original,
                dimensions[0] as u32,
                dimensions[1] as u32,
                image::imageops::FilterType::Triangle,
            );
            egui::ColorImage::from_rgba_unmultiplied(dimensions, resized.as_raw())
        };
        self.texture = Some((
            key,
            ctx.load_texture("capture", image, egui::TextureOptions::LINEAR),
        ));
    }

    fn at_cursor(id: u64) -> Self {
        let anchor = screen_clip::cursor_position();
        Self {
            id,
            task_id: None,
            anchor,
            work_area: screen_clip::work_area_at(anchor),
            opened_at: Instant::now(),
            pinned: false,
            show_source: false,
            copied: false,
            placed: None,
            shown: false,
            width: POPUP_WIDTH,
            height: 120.0,
            drag_origin: None,
            resize_origin: None,
            moved: false,
            user_sized: false,
            image_view: None,
            texture: None,
            result: TranslationResult::default(),
            pending: false,
            provisional: false,
            status: String::new(),
            escape_was_down: screen_clip::escape_down(),
            click_was_down: screen_clip::mouse_button_down(),
            close_requested: false,
        }
    }

    /// Top-left corner for a window of `size` physical pixels: below and right of the anchor,
    /// flipped to the other side when it would leave the monitor's work area.
    fn place(&self, size: egui::Vec2) -> egui::Pos2 {
        const GAP: f32 = 16.0;
        let [left, top, right, bottom] = self.work_area.map(|value| value as f32);
        let [x, y] = self.anchor.map(|value| value as f32);
        let mut corner = egui::pos2(x + GAP, y + GAP);
        if corner.x + size.x > right {
            corner.x = x - GAP - size.x;
        }
        if corner.y + size.y > bottom {
            corner.y = y - GAP - size.y;
        }
        egui::pos2(
            corner.x.clamp(left, (right - size.x).max(left)),
            corner.y.clamp(top, (bottom - size.y).max(top)),
        )
    }

    /// Where a window already on screen goes when its size changes: it keeps its corner,
    /// moved only as far as the work area requires, rather than jumping to the other side
    /// of the selection as a fresh placement might.
    fn settle(&self, corner: egui::Pos2, size: egui::Vec2) -> egui::Pos2 {
        let [left, top, right, bottom] = self.work_area.map(|value| value as f32);
        egui::pos2(
            corner.x.clamp(left, (right - size.x).max(left)),
            corner.y.clamp(top, (bottom - size.y).max(top)),
        )
    }

    fn place_avoiding(&self, size: egui::Vec2, occupied: &[egui::Rect]) -> egui::Pos2 {
        const GAP: f32 = 12.0;
        let base = self.place(size);
        if occupied.is_empty() {
            return base;
        }
        let [left, top, right, bottom] = self.work_area.map(|value| value as f32);
        let step_x = size.x + GAP;
        let step_y = size.y + GAP;
        let rings = (((right - left) / step_x)
            .max((bottom - top) / step_y)
            .ceil() as i32)
            .clamp(1, 64);
        for ring in 0..=rings {
            let r = ring as f32;
            let offsets = [
                egui::vec2(0.0, r * step_y),
                egui::vec2(0.0, -r * step_y),
                egui::vec2(r * step_x, 0.0),
                egui::vec2(-r * step_x, 0.0),
                egui::vec2(r * step_x, r * step_y),
                egui::vec2(-r * step_x, r * step_y),
                egui::vec2(r * step_x, -r * step_y),
                egui::vec2(-r * step_x, -r * step_y),
            ];
            for offset in offsets {
                let candidate = base + offset;
                let corner = egui::pos2(
                    candidate.x.clamp(left, (right - size.x).max(left)),
                    candidate.y.clamp(top, (bottom - size.y).max(top)),
                );
                let rect = egui::Rect::from_min_size(corner, size);
                if occupied
                    .iter()
                    .all(|other| !rect.intersects(other.expand(GAP / 2.0)))
                {
                    return corner;
                }
            }
        }
        base
    }

    /// Esc, or a click outside every popup once the snip's own mouse release has passed,
    /// closes an unpinned popup. Polled globally because the popup does not take keyboard focus.
    fn dismissed(&mut self, click_inside_any_popup: bool) -> bool {
        let escape = screen_clip::escape_down();
        let click = screen_clip::mouse_button_down();
        let escape_pressed = escape && !self.escape_was_down;
        let click_pressed = click && !self.click_was_down;
        self.escape_was_down = escape;
        self.click_was_down = click;
        if self.pinned {
            return false;
        }
        if escape_pressed {
            return true;
        }
        // A click that landed in any popup window counts as inside, so interacting with
        // one popup never dismisses its neighbours.
        click_pressed
            && self.opened_at.elapsed() > Duration::from_millis(400)
            && !click_inside_any_popup
    }

    /// Anchor a move to screen coordinates so moving the window cannot feed back into pointer delta.
    fn begin_drag(&mut self, cursor: [i32; 2]) {
        self.drag_origin = self.placed.map(|rect| (cursor, rect));
        self.moved |= self.drag_origin.is_some();
    }

    fn apply_drag(&mut self, ctx: &egui::Context, cursor: [i32; 2]) {
        let Some((origin, start_rect)) = self.drag_origin else {
            return;
        };
        let scale = ctx.pixels_per_point();
        let [left, top, right, bottom] = self.work_area.map(|value| value as f32);
        let delta = egui::vec2(
            (cursor[0] - origin[0]) as f32,
            (cursor[1] - origin[1]) as f32,
        );
        let mut corner = start_rect.min + delta;
        corner.x = corner.x.clamp(left, (right - start_rect.width()).max(left));
        corner.y = corner.y.clamp(top, (bottom - start_rect.height()).max(top));
        if self.placed.is_some_and(|rect| rect.min == corner) {
            return;
        }
        self.placed = Some(egui::Rect::from_min_size(corner, start_rect.size()));
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
            corner.x / scale,
            corner.y / scale,
        )));
    }

    /// Resize from the physical cursor origin, keeping the top-left corner fixed.
    fn begin_resize(&mut self, cursor: [i32; 2]) {
        self.resize_origin = self.placed.map(|rect| (cursor, rect));
        self.user_sized |= self.resize_origin.is_some();
        self.moved |= self.resize_origin.is_some();
    }

    fn apply_resize(&mut self, ctx: &egui::Context, cursor: [i32; 2]) {
        let Some((origin, start_rect)) = self.resize_origin else {
            return;
        };
        let scale = ctx.pixels_per_point();
        let [_, _, right, bottom] = self.work_area.map(|value| value as f32);
        let min = egui::vec2(240.0, 140.0) * scale;
        let max = egui::vec2(
            (right - start_rect.left()).max(min.x),
            (bottom - start_rect.top()).max(min.y),
        );
        let delta = egui::vec2(
            (cursor[0] - origin[0]) as f32,
            (cursor[1] - origin[1]) as f32,
        );
        let size = (start_rect.size() + delta).clamp(min, max);
        if self.placed.is_some_and(|rect| rect.size() == size) {
            return;
        }
        self.placed = Some(egui::Rect::from_min_size(start_rect.min, size));
        self.width = size.x / scale;
        self.height = size.y / scale;
        ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size / scale));
    }

    /// Draw one popup's content: header controls, translation, and the resize grip.
    /// `target` names the language the translation went into, when that can vary.
    fn draw(
        &mut self,
        ui: &mut egui::Ui,
        palette: &Palette,
        image_size: Option<egui::Vec2>,
        target: Option<&str>,
        speech: &PopupSpeech,
        actions: &mut PopupActions,
    ) {
        let translated = self.result.translated_text.trim().to_owned();
        let source = self.result.source_text.trim().to_owned();
        let layout = self.result.layout.clone();
        let pending = self.pending;
        let action = |ui: &mut egui::Ui, text: &str, color: Color32| {
            ui.add(egui::Button::new(RichText::new(text).small().color(color)).frame(false))
                .clicked()
        };
        let speak_source = |ui: &mut egui::Ui, actions: &mut PopupActions| {
            let (label, color) = match speech.source {
                tts::Status::Active => ("停止朗读", palette.accent),
                tts::Status::Failed(_) => ("朗读原文", palette.danger),
                tts::Status::Idle => ("朗读原文", palette.accent),
            };
            if action(ui, label, color) {
                actions.toggle_speech = Some(tts::Part::Source);
            }
        };
        // An invisible drag strip under the header row: registered first so the row's
        // controls stay clickable while the empty space moves the window. It reaches a
        // little past the row; widgets drawn on top keep their own interactions.
        let strip =
            egui::Rect::from_min_size(ui.cursor().min, egui::vec2(ui.available_width(), 28.0));
        let drag = ui.interact(strip, ui.id().with("title_drag"), egui::Sense::drag());
        if drag.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        if drag.drag_started_by(egui::PointerButton::Primary) {
            self.begin_drag(screen_clip::cursor_position());
        }
        if drag.dragged() {
            self.apply_drag(ui.ctx(), screen_clip::cursor_position());
        }
        if drag.drag_stopped_by(egui::PointerButton::Primary) {
            self.drag_origin = None;
        }
        ui.horizontal(|ui| {
            if pending {
                ui.add(egui::Spinner::new().size(12.0).color(palette.accent));
                ui.label(RichText::new(&self.status).small().color(palette.muted));
            } else if layout.is_some() {
                let mut image_view = self.image_view == Some(true);
                if segmented(
                    ui,
                    palette,
                    &mut image_view,
                    &[(true, "图片"), (false, "文字")],
                ) {
                    self.image_view = Some(image_view);
                }
            } else {
                ui.label(RichText::new("译文").small().strong().color(palette.muted));
            }
            if let Some(target) = target.filter(|_| !pending && !translated.is_empty()) {
                ui.label(
                    RichText::new(format!("译成{target}"))
                        .small()
                        .color(palette.muted),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if action(ui, "关闭", palette.muted) {
                    actions.close = true;
                }
                if !translated.is_empty() {
                    let label = if self.copied { "已复制" } else { "复制" };
                    if action(ui, label, palette.accent) {
                        ui.ctx().copy_text(translated.clone());
                        self.copied = true;
                    }
                    let (label, color) = match speech.translation {
                        tts::Status::Active => ("停止", palette.accent),
                        tts::Status::Failed(_) => ("朗读", palette.danger),
                        tts::Status::Idle => ("朗读", palette.muted),
                    };
                    if action(ui, label, color) {
                        actions.toggle_speech = Some(tts::Part::Translation);
                    }
                }
                let (label, color) = if self.pinned {
                    ("已固定", palette.accent)
                } else {
                    ("固定", palette.muted)
                };
                if action(ui, label, color) {
                    self.pinned = !self.pinned;
                }
                if action(ui, "主窗口", palette.muted) {
                    actions.open_main_window = true;
                }
            });
        });
        if let Some(error) = speech.error() {
            ui.add(egui::Label::new(RichText::new(error).small().color(palette.danger)).wrap());
        }
        ui.add_space(4.0);

        self.prepare_texture(ui.ctx(), layout.as_ref().filter(|_| !pending), image_size);
        if let (Some(size), Some(page), Some((_, texture))) = (image_size, &layout, &self.texture) {
            paint_translated_capture(ui, texture, page, size, self.show_source);
            if !self.show_source
                && !translated.is_empty()
                && !page.blocks.iter().any(|block| block.translated.is_some())
            {
                ui.add_space(6.0);
                ui.label(RichText::new("译文").small().color(palette.muted));
                egui::ScrollArea::vertical()
                    .max_height(120.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(translated.as_str())
                                .wrap()
                                .selectable(true),
                        );
                    });
            }
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let label = if self.show_source {
                    "显示译文"
                } else {
                    "对比原图"
                };
                if action(ui, label, palette.accent) {
                    self.show_source = !self.show_source;
                }
                if !source.is_empty() {
                    speak_source(ui, actions);
                }
            });
            return;
        }

        // A user-sized window lets the text use the space it gained; the auto-fitted
        // window keeps its compact default.
        let max_text_height = if self.user_sized {
            (ui.available_height() - 64.0).max(60.0)
        } else {
            POPUP_MAX_HEIGHT - 110.0
        };
        egui::ScrollArea::vertical()
            .max_height(max_text_height)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                if !translated.is_empty() {
                    ui.add(
                        egui::Label::new(RichText::new(translated.as_str()).color(palette.text))
                            .wrap()
                            .selectable(true),
                    );
                    if let Some(entry) = &self.result.dictionary {
                        ui.add_space(6.0);
                        ui.separator();
                        draw_dictionary(ui, palette, entry);
                    }
                } else if pending {
                    if source.is_empty() {
                        ui.label(RichText::new(&self.status).color(palette.muted));
                    } else {
                        ui.label(RichText::new("识别原文").small().color(palette.muted));
                        ui.add(
                            egui::Label::new(RichText::new(source.as_str()).color(palette.text))
                                .wrap()
                                .selectable(true),
                        );
                    }
                } else {
                    ui.add(
                        egui::Label::new(RichText::new(&self.status).color(palette.danger)).wrap(),
                    );
                }
                if !pending && self.show_source && !source.is_empty() {
                    ui.add_space(6.0);
                    ui.separator();
                    ui.add(
                        egui::Label::new(RichText::new(source.as_str()).color(palette.muted))
                            .wrap()
                            .selectable(true),
                    );
                }
            });

        if !source.is_empty() {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                let label = if self.show_source {
                    "收起原文"
                } else {
                    "查看原文"
                };
                if action(ui, label, palette.accent) {
                    self.show_source = !self.show_source;
                }
                speak_source(ui, actions);
            });
        }

        // The bottom-right grip resizes the window; auto-fitted windows grow with
        // their content instead, so the grip only matters after (or for) enlarging.
        ui.add_space(2.0);
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (rect, grip) =
                    ui.allocate_exact_size(egui::vec2(18.0, 10.0), egui::Sense::drag());
                if grip.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeNwSe);
                }
                let corner = rect.right_bottom();
                for step in 0..3u8 {
                    let offset = 3.0 + f32::from(step) * 3.0;
                    ui.painter().line_segment(
                        [
                            corner - egui::vec2(offset, 0.0),
                            corner - egui::vec2(0.0, offset),
                        ],
                        Stroke::new(1.0_f32, palette.border),
                    );
                }
                if grip.drag_started_by(egui::PointerButton::Primary) {
                    self.begin_resize(screen_clip::cursor_position());
                }
                if grip.dragged() {
                    self.apply_resize(ui.ctx(), screen_clip::cursor_position());
                }
                if grip.drag_stopped_by(egui::PointerButton::Primary) {
                    self.resize_origin = None;
                }
            });
        });
    }
}

#[derive(Default, Clone, Serialize, Deserialize)]
struct TranslationResult {
    source_text: String,
    translated_text: String,
    /// Block positions and translations for painting the result over the screenshot.
    layout: Option<Arc<layout::ImageLayout>>,
    /// Phonetics and parts of speech when the source is a single English word.
    dictionary: Option<Arc<dictionary::Entry>>,
}

impl TranslationResult {
    /// The main window needs only text; the file remains available for retranslation.
    fn release_unused_image(&mut self, popup_open: bool, busy: bool) {
        if !popup_open && !busy {
            self.layout = None;
        }
    }
}

#[derive(Serialize, Deserialize)]
enum RequestResult {
    Source(String),
    Success(TranslationResult),
    Error(String),
}

struct OcrTranslateApp {
    client: ipc::Client,
    tray_available: bool,
    lightweight: bool,
    suspending: bool,
    main_visible: bool,
    exit_requested: bool,
    config: AppConfig,
    config_path: PathBuf,
    result: TranslationResult,
    selected_file: Option<PathBuf>,
    temporary_capture: Option<PathBuf>,
    active_task_id: Option<u64>,
    request_rx: Option<Receiver<RequestResult>>,
    capture_rx: Option<Receiver<Result<PathBuf, String>>>,
    active_hotkey: Option<screen_clip::Hotkey>,
    shortcut_draft: screen_clip::Hotkey,
    hotkey_capture_active: bool,
    paste_key_down: bool,
    popups: Vec<ResultPopup>,
    popup_counter: u64,
    volcengine_usage: Option<Result<volcengine::Usage, String>>,
    usage_rx: Option<Receiver<Result<volcengine::Usage, String>>>,
    status: String,
    settings_open: bool,
    show_api_key: bool,
    show_volcengine_keys: bool,
    show_azure_key: bool,
    ocr_languages: Vec<String>,
    /// Languages of the installed Windows voices, listed once the speech settings are shown.
    speech_languages: Option<Vec<String>>,
    speaker: tts::Speaker,
}

impl OcrTranslateApp {
    fn new(cc: &eframe::CreationContext<'_>, config_path: PathBuf) -> Self {
        let ctx = &cc.egui_ctx;
        tray::AppTray::bind_window(cc);
        let client = ipc::Client::connect(ctx.clone());
        let config_existed = config_path.exists();
        let mut config = load_config(&config_path);
        let (plaintext_secrets, undecryptable_secrets) = config.reveal_secrets();
        config.font_size = config
            .font_size
            .clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end());
        let shortcut = screen_clip::Hotkey::parse(&config.shortcut).unwrap_or_default();
        config.shortcut = shortcut.to_string();
        apply_appearance(ctx, &config);
        net::set_proxy(config.proxy());
        let ocr_languages = windows_ocr::available_languages();
        if config.ocr_language != "auto"
            && !ocr_languages.iter().any(|tag| tag == &config.ocr_language)
        {
            config.ocr_language = "auto".into();
        }
        let mut app = Self {
            client,
            tray_available: true,
            lightweight: false,
            suspending: false,
            main_visible: false,
            exit_requested: false,
            config,
            config_path,
            result: TranslationResult::default(),
            selected_file: None,
            temporary_capture: None,
            active_task_id: None,
            request_rx: None,
            capture_rx: None,
            active_hotkey: None,
            shortcut_draft: shortcut,
            hotkey_capture_active: false,
            paste_key_down: false,
            popups: Vec::new(),
            popup_counter: 0,
            volcengine_usage: None,
            usage_rx: None,
            status: format!("准备就绪。按 {shortcut} 框选屏幕。"),
            // First run lands on settings; afterwards go straight to translating.
            settings_open: !config_existed,
            show_api_key: false,
            show_volcengine_keys: false,
            show_azure_key: false,
            ocr_languages,
            speech_languages: None,
            speaker: tts::Speaker::default(),
        };
        if undecryptable_secrets {
            app.status =
                "已保存的密钥无法解密（可能来自其他电脑或 Windows 账户），请在设置中重新填写。"
                    .into();
        } else if plaintext_secrets {
            // Re-save right away so keys written in plaintext by older versions get encrypted.
            if let Err(error) = app.write_config() {
                app.status = error;
            }
        }
        app
    }

    fn capture_busy(&self) -> bool {
        self.capture_rx.is_some() || self.hotkey_capture_active
    }

    fn busy(&self) -> bool {
        self.request_rx.is_some() || self.capture_busy()
    }

    /// Add a new result popup at the current cursor, or at `anchor` when the capture
    /// reported where the selection ended.
    fn spawn_popup(&mut self, anchor: Option<[i32; 2]>, pending: bool, provisional: bool) {
        self.popup_counter += 1;
        let mut popup = ResultPopup::at_cursor(self.popup_counter);
        if let Some(anchor) = anchor {
            popup.anchor = anchor;
            popup.work_area = screen_clip::work_area_at(anchor);
        }
        popup.pending = pending;
        popup.provisional = provisional;
        popup.status = self.status.clone();
        self.popups.push(popup);
    }

    fn reveal_main_window(&mut self, ctx: &egui::Context) {
        self.main_visible = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn apply_close_behavior(&mut self, behavior: CloseBehavior, ctx: &egui::Context) {
        self.popups.clear();
        match behavior {
            CloseBehavior::Tray => {
                self.main_visible = false;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            }
            CloseBehavior::Exit => {
                let _ = self.client.send(ipc::Command::Quit);
                self.exit_requested = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn begin_remote_request(&mut self) {
        let (_tx, rx) = mpsc::channel();
        self.request_rx = Some(rx);
        // Completion arrives through the private IPC channel, not this receiver.
        self.result = TranslationResult::default();
    }

    fn show_view(&mut self, view: ipc::View, ctx: &egui::Context) {
        match view {
            ipc::View::Main | ipc::View::Settings => {
                self.lightweight = false;
                self.settings_open = matches!(view, ipc::View::Settings);
                self.reveal_main_window(ctx);
            }
            ipc::View::Popup => match self.popups.last_mut() {
                // A provisional popup is still waiting for this capture's outcome event;
                // refresh its anchor instead of opening a second window.
                Some(popup) if popup.provisional => {
                    popup.anchor = screen_clip::cursor_position();
                    popup.work_area = screen_clip::work_area_at(popup.anchor);
                }
                _ => self.spawn_popup(None, true, true),
            },
        }
    }

    fn restore_pixels(&mut self) {
        if let (Some(page), Some(path)) = (&mut self.result.layout, &self.selected_file) {
            match image::open(path).map(|image| image.to_rgba8()) {
                Ok(image) if image.dimensions() == (page.width, page.height) => {
                    Arc::make_mut(page).rgba = image.into_raw();
                }
                _ => {
                    self.result.layout = None;
                    self.status = "图片无法重新加载，已保留文字翻译。".into();
                }
            }
        }
    }

    fn poll_backend(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.client.events.try_recv() {
            match event {
                ipc::Event::Init {
                    session,
                    view,
                    lightweight,
                    tray_available,
                    active_hotkey,
                    busy,
                } => {
                    if let Some(config) = session.config {
                        self.config = config;
                        apply_appearance(ctx, &self.config);
                        net::set_proxy(self.config.proxy());
                    }
                    self.shortcut_draft =
                        screen_clip::Hotkey::parse(&self.config.shortcut).unwrap_or_default();
                    self.active_hotkey = active_hotkey;
                    self.active_task_id = session.task_id;
                    self.result = session.result;
                    self.selected_file = session.selected_file;
                    self.temporary_capture = session.temporary_capture;
                    if !session.status.is_empty() {
                        self.status = session.status;
                    }
                    self.lightweight = lightweight;
                    self.tray_available = tray_available;
                    self.restore_pixels();
                    self.show_view(view, ctx);
                    if let (Some(anchor), Some(popup)) = (session.anchor, self.popups.last_mut()) {
                        popup.anchor = anchor;
                        popup.work_area = screen_clip::work_area_at(anchor);
                    }
                    if !busy && matches!(view, ipc::View::Popup) {
                        if let Some(popup) =
                            self.popups.last_mut().filter(|popup| popup.provisional)
                        {
                            restore_popup_result(popup, self.result.clone(), &self.status);
                        }
                    }
                    if busy {
                        let (_tx, rx) = mpsc::channel();
                        self.request_rx = Some(rx);
                        for popup in &mut self.popups {
                            popup.pending = true;
                            popup.task_id = self.active_task_id;
                        }
                    }
                }
                ipc::Event::Show(view) => {
                    self.suspending = false;
                    self.show_view(view, ctx);
                }
                ipc::Event::Lightweight(enabled) => {
                    self.suspending = false;
                    self.lightweight = enabled;
                }
                ipc::Event::Hotkey(event) => self.handle_hotkey(event),
                ipc::Event::TaskStarted(task_id) => {
                    self.active_task_id = Some(task_id);
                }
                ipc::Event::Accepted {
                    task_id,
                    path,
                    popup,
                    anchor,
                } => {
                    self.active_task_id = Some(task_id);
                    self.hotkey_capture_active = false;
                    self.capture_rx = None;
                    self.selected_file = Some(path.clone());
                    self.temporary_capture = Some(path);
                    self.begin_remote_request();
                    self.status = "正在识别并翻译…".into();
                    if popup {
                        retain_pinned_and_provisional_popups(&mut self.popups);
                        match self.popups.iter_mut().rev().find(|popup| popup.provisional) {
                            Some(popup) => {
                                popup.task_id = Some(task_id);
                                popup.anchor = anchor;
                                popup.work_area = screen_clip::work_area_at(anchor);
                                popup.pending = true;
                                popup.provisional = false;
                                popup.status = self.status.clone();
                            }
                            None => {
                                self.spawn_popup(Some(anchor), true, false);
                                if let Some(popup) = self.popups.last_mut() {
                                    popup.task_id = Some(task_id);
                                }
                            }
                        }
                    } else {
                        self.reveal_main_window(ctx);
                    }
                }
                ipc::Event::Request { task_id, result } if self.active_task_id == Some(task_id) => {
                    match result {
                        RequestResult::Source(source) => {
                            self.result.source_text = source.clone();
                            self.status = "识别完成，正在翻译…".into();
                            for popup in self
                                .popups
                                .iter_mut()
                                .filter(|popup| popup.task_id == Some(task_id) && popup.pending)
                            {
                                popup.result.source_text = source.clone();
                                popup.status = self.status.clone();
                            }
                        }
                        RequestResult::Success(result) => {
                            self.request_rx = None;
                            self.capture_rx = None;
                            self.result = result;
                            self.restore_pixels();
                            self.status = "翻译完成。".into();
                            if let Some(popup) = self
                                .popups
                                .iter_mut()
                                .find(|popup| popup.task_id == Some(task_id))
                            {
                                popup.pending = false;
                                popup.provisional = false;
                                popup.result = self.result.clone();
                                popup.status = self.status.clone();
                            }
                            self.active_task_id = None;
                        }
                        RequestResult::Error(error) => {
                            self.request_rx = None;
                            self.status = error.clone();
                            if let Some(popup) = self
                                .popups
                                .iter_mut()
                                .find(|popup| popup.task_id == Some(task_id))
                            {
                                popup.pending = false;
                                popup.provisional = false;
                                popup.status = error;
                            }
                            self.active_task_id = None;
                        }
                    }
                }
                ipc::Event::Request { .. } => {}
                ipc::Event::Quit => {
                    self.exit_requested = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn finish_frame(&mut self, ctx: &egui::Context) {
        let busy = self.busy();
        self.result
            .release_unused_image(!self.popups.is_empty(), busy);
        let popups = &self.popups;
        self.speaker.retain(|window| {
            window == tts::MAIN
                || window == tts::SAMPLE
                || popups.iter().any(|popup| popup.id == window)
        });
        // Reading aloud keeps the interface alive; it suspends once the speech has ended.
        if self.lightweight
            && !busy
            && self.popups.is_empty()
            && !self.speaker.active()
            && !self.suspending
        {
            let session = ipc::Session {
                config: Some(self.config.clone()),
                result: self.result.clone(),
                selected_file: self.selected_file.clone(),
                temporary_capture: self.temporary_capture.clone(),
                status: self.status.clone(),
                anchor: None,
                task_id: self.active_task_id,
            };
            if let Err(error) = self.client.send(ipc::Command::Suspend(Box::new(session))) {
                self.status = error;
            } else {
                self.suspending = true;
            }
        }
        if self.suspending {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    /// Reads aloud the part of a result the button `key` belongs to, `source` or its
    /// `translation`, or stops it when that button is already speaking.
    fn toggle_speech(
        &mut self,
        key: tts::Key,
        source: &str,
        translation: &str,
        ctx: &egui::Context,
    ) {
        let (text, voice) = match key.1 {
            tts::Part::Source => (source, tts::source_voice(&self.config.ocr_language, source)),
            tts::Part::Translation => {
                // The source tells which way it was translated; Google's translation codes
                // double as the voice table's language keys.
                let target =
                    target_for(&self.config, source).unwrap_or(&self.config.target_language);
                let code = free_translate::target_code(target, TranslationMode::Google).ok();
                (translation, tts::voice_for(code, translation))
            }
        };
        let request = tts::Request {
            engine: self.config.tts_engine,
            voice,
            text: text.trim().to_owned(),
            rate: self
                .config
                .tts_rate
                .clamp(*tts::RATE_RANGE.start(), *tts::RATE_RANGE.end()),
            azure_key: self.config.azure_speech_key.clone(),
            azure_region: self.config.azure_speech_region.clone(),
        };
        let ctx = ctx.clone();
        self.speaker.toggle(key, request, move || tray::wake(&ctx));
    }

    fn shortcut_label(&self) -> String {
        self.active_hotkey
            .map_or_else(|| "全局快捷键".into(), |hotkey| hotkey.to_string())
    }

    fn write_config(&self) -> Result<(), String> {
        const FAILED: &str = "设置保存失败，请检查应用数据目录权限。";
        let json = serde_json::to_string_pretty(&self.config.protected()?)
            .map_err(|_| FAILED.to_string())?;
        if let Some(directory) = self.config_path.parent() {
            fs::create_dir_all(directory).map_err(|_| FAILED.to_string())?;
        }
        fs::write(&self.config_path, json).map_err(|_| FAILED.to_string())
    }

    fn save_config(&mut self) {
        if let Err(error) = self.shortcut_draft.validate() {
            self.status = error;
            return;
        }
        if self.config.proxy_mode == net::ProxyMode::Custom {
            if let Err(error) = net::validate_proxy_url(&self.config.proxy_url) {
                self.status = error;
                return;
            }
        }
        net::set_proxy(self.config.proxy());
        self.config.shortcut = self.shortcut_draft.to_string();
        self.status = match self.write_config() {
            Ok(()) => "设置已保存。".into(),
            Err(error) => error,
        };
        if let Err(error) = self
            .client
            .send(ipc::Command::Config(Box::new(self.config.clone())))
        {
            self.status = error;
        }
    }

    fn choose_image(&mut self, ctx: &egui::Context) {
        let picked = rfd::FileDialog::new()
            .add_filter("图片", &["png", "jpg", "jpeg", "webp", "bmp"])
            .pick_file();
        if let Some(path) = picked {
            self.temporary_capture = None;
            self.selected_file = Some(path.clone());
            self.start_translation(path, ctx);
        }
    }

    fn start_screen_capture(&mut self, ctx: &egui::Context) {
        if self.capture_busy() {
            return;
        }
        self.status = "请在屏幕上拖动鼠标，框选需要翻译的区域。按 Esc 取消。".into();
        self.hotkey_capture_active = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
        if let Err(error) = self
            .client
            .send(ipc::Command::Capture(Box::new(self.config.clone())))
        {
            self.hotkey_capture_active = false;
            self.status = error;
            self.reveal_main_window(ctx);
        }
    }

    fn handle_hotkey(&mut self, event: screen_clip::HotkeyEvent) {
        match event {
            screen_clip::HotkeyEvent::Ready(hotkey) => {
                let previous = self.active_hotkey.replace(hotkey);
                if previous.is_some_and(|previous| previous != hotkey) {
                    self.status = format!("全局快捷键已更新为 {hotkey}。");
                }
            }
            screen_clip::HotkeyEvent::Busy => {
                self.status = "截图选择器仍在运行，请先完成或取消当前框选。".into();
            }
            screen_clip::HotkeyEvent::Started => {
                self.hotkey_capture_active = true;
                self.status = "请在屏幕上拖动鼠标，框选需要翻译的区域。按 Esc 取消。".into();
            }
            screen_clip::HotkeyEvent::Captured(Ok(path)) => {
                self.selected_file = Some(path);
            }
            screen_clip::HotkeyEvent::Captured(Err(error)) => {
                self.hotkey_capture_active = false;
                if error == screen_clip::CANCELLED {
                    discard_provisional_popups(&mut self.popups);
                } else {
                    self.status = error.clone();
                    match self.popups.last_mut() {
                        Some(popup) if popup.provisional => {
                            popup.pending = false;
                            popup.provisional = false;
                            popup.status = self.status.clone();
                        }
                        _ => self.spawn_popup(None, false, false),
                    }
                }
                self.status = error;
            }
            screen_clip::HotkeyEvent::Failed { message, active } => {
                self.status = message;
                self.active_hotkey = active;
                // The listener fell back to the previous shortcut; keep the saved setting
                // in step so the next launch does not retry the rejected one.
                if let Some(active) = active {
                    if self.config.shortcut != active.to_string() {
                        self.config.shortcut = active.to_string();
                        self.shortcut_draft = active;
                        let _ = self.write_config();
                    }
                }
            }
        }
    }

    /// Hotkey captures leave the main window alone and show their result in the popup.
    fn accept_capture(&mut self, path: PathBuf, reveal_main_window: bool, ctx: &egui::Context) {
        self.temporary_capture = Some(path.clone());
        self.selected_file = Some(path.clone());
        self.settings_open = false;
        if reveal_main_window {
            self.reveal_main_window(ctx);
        }
        self.start_translation(path, ctx);
    }

    fn paste_image(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        match screen_clip::paste_clipboard_image() {
            Ok(Some(path)) => self.accept_capture(path, true, ctx),
            Ok(None) => {}
            Err(error) => self.status = error,
        }
    }

    /// Returns false and opens the settings page when the selected channel lacks credentials.
    fn ensure_credentials(&mut self) -> bool {
        let config = &self.config;
        let missing = match config.mode {
            TranslationMode::Vision if config.api_key.trim().is_empty() => {
                Some("请先填写 API Key。")
            }
            TranslationMode::Vision
                if config.api_url.trim().is_empty() || config.model.trim().is_empty() =>
            {
                Some("请先填写 API URL 和模型名称。")
            }
            TranslationMode::Volcengine => {
                let (access_key, secret_key) = volcengine_credentials(config);
                (access_key.trim().is_empty() || secret_key.trim().is_empty())
                    .then_some("请先在设置中填写火山引擎 Access Key 和 Secret Key。")
            }
            _ => None,
        };
        if let Some(message) = missing {
            self.status = message.into();
            self.settings_open = true;
        }
        missing.is_none()
    }

    fn start_translation(&mut self, path: PathBuf, ctx: &egui::Context) {
        if !self.ensure_credentials() {
            return;
        }

        self.begin_remote_request();
        self.status = "正在识别并翻译…".into();
        ctx.request_repaint();

        let temporary = self.temporary_capture.as_ref() == Some(&path);
        if let Err(error) = self.client.send(ipc::Command::Image {
            config: Box::new(self.config.clone()),
            path,
            temporary,
        }) {
            self.request_rx = None;
            self.status = error;
        }
    }

    fn start_text_translation(&mut self, ctx: &egui::Context) {
        let source = self.result.source_text.trim().to_owned();
        if source.is_empty() {
            self.status = "请先在原文框输入或粘贴文字。".into();
            return;
        }
        if !self.ensure_credentials() {
            return;
        }

        self.begin_remote_request();
        self.result.source_text = source.clone();
        self.result.layout = None;
        self.result.translated_text.clear();
        self.status = "正在翻译…".into();
        ctx.request_repaint();

        if let Err(error) = self.client.send(ipc::Command::Text {
            config: Box::new(self.config.clone()),
            source,
        }) {
            self.request_rx = None;
            self.status = error;
        }
    }

    fn draw_settings(&mut self, ui: &mut egui::Ui) {
        let palette = Palette::of(self.config.theme);
        let width = ui.available_width().min(680.0);
        ui.horizontal(|ui| {
            ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
            ui.vertical(|ui| {
                ui.set_width(width);
                ui.add_space(4.0);
                self.draw_appearance_card(ui, &palette);
                ui.add_space(14.0);
                self.draw_channel_card(ui, &palette);
                ui.add_space(14.0);
                self.draw_speech_card(ui, &palette);
                ui.add_space(14.0);
                self.draw_shortcut_card(ui, &palette);
                ui.add_space(14.0);
                self.draw_proxy_card(ui, &palette);
                ui.add_space(18.0);
                ui.horizontal(|ui| {
                    if ui.add(primary_button(&palette, "保存设置")).clicked() {
                        self.save_config();
                    }
                    ui.add_space(8.0);
                    hint(
                        ui,
                        &palette,
                        "配置保存在当前用户的应用数据目录，密钥已加密。",
                    );
                });
                ui.add_space(18.0);
                draw_about_card(ui, &palette);
                ui.add_space(12.0);
            });
        });
    }

    fn draw_appearance_card(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        card(ui, palette, |ui| {
            section_title(
                ui,
                palette,
                "外观",
                "主题与字号会立即预览，保存后下次启动仍然生效。",
            );
            egui::Grid::new("appearance")
                .num_columns(2)
                .spacing([24.0, 14.0])
                .show(ui, |ui| {
                    field_label(ui, palette, "主题");
                    if segmented(
                        ui,
                        palette,
                        &mut self.config.theme,
                        &[(ThemeMode::Day, "日间"), (ThemeMode::Night, "夜间")],
                    ) {
                        apply_appearance(ui.ctx(), &self.config);
                    }
                    ui.end_row();

                    field_label(ui, palette, "字号");
                    if ui
                        .add(
                            egui::Slider::new(&mut self.config.font_size, FONT_SIZE_RANGE)
                                .suffix(" px"),
                        )
                        .changed()
                    {
                        apply_appearance(ui.ctx(), &self.config);
                    }
                    ui.end_row();

                    field_label(ui, palette, "关闭主窗口时");
                    egui::ComboBox::from_id_salt("close_behavior")
                        .selected_text(self.config.close_behavior.label())
                        .show_ui(ui, |ui| {
                            for behavior in [CloseBehavior::Tray, CloseBehavior::Exit] {
                                ui.selectable_value(
                                    &mut self.config.close_behavior,
                                    behavior,
                                    behavior.label(),
                                );
                            }
                        });
                    ui.end_row();
                });
        });
    }

    fn draw_channel_card(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        card(ui, palette, |ui| {
            section_title(ui, palette, "翻译", "选择翻译渠道和语言。");
            field_label(ui, palette, "翻译渠道");
            egui::ComboBox::from_id_salt("translation_mode")
                .width(ui.available_width())
                .selected_text(self.config.mode.label())
                .show_ui(ui, |ui| {
                    for mode in [
                        TranslationMode::Tencent,
                        TranslationMode::Google,
                        TranslationMode::Volcengine,
                        TranslationMode::Vision,
                    ] {
                        ui.selectable_value(&mut self.config.mode, mode, mode.label());
                    }
                });
            match self.config.mode {
                TranslationMode::Tencent | TranslationMode::Google => {
                    hint(
                        ui,
                        palette,
                        "免 Key 渠道使用第三方网页接口，可用性会随服务方变化。",
                    );
                }
                TranslationMode::Volcengine => {
                    let (access_key, secret_key) = volcengine_credentials(&self.config);
                    let configured = !access_key.trim().is_empty() && !secret_key.trim().is_empty();
                    if configured {
                        ui.add_space(8.0);
                        self.draw_volcengine_usage(ui, palette);
                    } else {
                        hint(ui, palette, "火山凭证尚未配置，填写后点击“保存设置”。");
                    }
                }
                TranslationMode::Vision => {}
            }
            ui.add_space(14.0);

            if self.config.mode == TranslationMode::Vision {
                field_label(ui, palette, "API URL");
                ui.add(text_field(&mut self.config.api_url).desired_width(f32::INFINITY));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    field_label(ui, palette, "API Key");
                    ui.checkbox(&mut self.show_api_key, "显示");
                });
                ui.add(
                    text_field(&mut self.config.api_key)
                        .desired_width(f32::INFINITY)
                        .password(!self.show_api_key),
                );
                ui.add_space(10.0);
                field_label(ui, palette, "模型");
                ui.add(text_field(&mut self.config.model).desired_width(f32::INFINITY));
                ui.add_space(10.0);
            } else {
                if self.config.mode == TranslationMode::Volcengine {
                    ui.horizontal(|ui| {
                        field_label(ui, palette, "Access Key (AK)");
                        ui.checkbox(&mut self.show_volcengine_keys, "显示密钥");
                    });
                    ui.add(
                        text_field(&mut self.config.volcengine_access_key)
                            .desired_width(f32::INFINITY)
                            .password(!self.show_volcengine_keys),
                    );
                    ui.add_space(10.0);
                    field_label(ui, palette, "Secret Key (SK)");
                    ui.add(
                        text_field(&mut self.config.volcengine_secret_key)
                            .desired_width(f32::INFINITY)
                            .password(!self.show_volcengine_keys),
                    );
                    hint(
                        ui,
                        palette,
                        "开发环境也兼容 VOLCENGINE_ACCESS_KEY / VOLCENGINE_SECRET_KEY 环境变量。",
                    );
                    ui.add_space(10.0);
                }

                field_label(ui, palette, "原文识别语言");
                egui::ComboBox::from_id_salt("ocr_language")
                    .width(ui.available_width())
                    .selected_text(if self.config.ocr_language == "auto" {
                        "自动（系统语言）"
                    } else {
                        &self.config.ocr_language
                    })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(
                            &mut self.config.ocr_language,
                            "auto".into(),
                            "自动（系统语言）",
                        );
                        for language in &self.ocr_languages {
                            ui.selectable_value(
                                &mut self.config.ocr_language,
                                language.clone(),
                                language,
                            );
                        }
                    });
                if self.ocr_languages.is_empty() {
                    hint(
                        ui,
                        palette,
                        "未检测到 Windows OCR 语言包，请在系统语言设置中安装。",
                    );
                }
                ui.add_space(10.0);
            }

            let free_form = self.config.mode == TranslationMode::Vision;
            field_label(ui, palette, "目标语言");
            language_field(
                ui,
                "target_language",
                &mut self.config.target_language,
                free_form,
            );
            ui.add_space(10.0);
            ui.checkbox(&mut self.config.two_way, "智能翻译方向")
                .on_hover_text("中文原文译成英文、外文原文译成中文这类互译，不用来回切换目标语言");
            if self.config.two_way {
                ui.add_space(6.0);
                field_label(ui, palette, "原文已是目标语言时译成");
                language_field(
                    ui,
                    "second_language",
                    &mut self.config.second_language,
                    free_form,
                );
                hint(ui, palette, &two_way_hint(&self.config));
            }
        });
    }

    fn draw_speech_card(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        card(ui, palette, |ui| {
            section_title(
                ui,
                palette,
                "朗读",
                "在悬浮窗或主窗口朗读原文和译文：译文按目标语言、原文按文字和识别语言选择语音。",
            );
            field_label(ui, palette, "朗读引擎");
            egui::ComboBox::from_id_salt("tts_engine")
                .width(ui.available_width())
                .selected_text(self.config.tts_engine.label())
                .show_ui(ui, |ui| {
                    for engine in tts::Engine::ALL {
                        ui.selectable_value(&mut self.config.tts_engine, engine, engine.label());
                    }
                });
            match self.config.tts_engine {
                tts::Engine::Windows => {
                    let languages = self
                        .speech_languages
                        .get_or_insert_with(tts::installed_languages);
                    if languages.is_empty() {
                        hint(
                            ui,
                            palette,
                            "未检测到 Windows 语音，请在 Windows 设置 → 时间和语言 → 语音 中添加。",
                        );
                    } else {
                        hint(
                            ui,
                            palette,
                            &format!(
                                "离线朗读，不上传文字。已安装语音：{}。缺少的语言可在 Windows 设置 → 时间和语言 → 语音 中添加。",
                                languages.join("、")
                            ),
                        );
                    }
                }
                tts::Engine::Google => hint(
                    ui,
                    palette,
                    "免 Key 网页接口，译文会发送到 Google；可能受限流或网络影响，不支持调节语速。",
                ),
                tts::Engine::Azure => {
                    hint(
                        ui,
                        palette,
                        "在 Azure 门户创建“语音服务”资源并选择免费 F0 定价层：每月 50 万字符神经语音，用完后限流，不会扣费。",
                    );
                    ui.add_space(10.0);
                    ui.horizontal(|ui| {
                        field_label(ui, palette, "密钥");
                        ui.checkbox(&mut self.show_azure_key, "显示");
                    });
                    ui.add(
                        text_field(&mut self.config.azure_speech_key)
                            .desired_width(f32::INFINITY)
                            .password(!self.show_azure_key),
                    );
                    ui.add_space(10.0);
                    field_label(ui, palette, "区域");
                    ui.add(
                        text_field(&mut self.config.azure_speech_region)
                            .desired_width(f32::INFINITY),
                    );
                    hint(
                        ui,
                        palette,
                        "填写资源所在区域的英文代码，例如 eastasia、eastus。",
                    );
                }
            }
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                field_label(ui, palette, "语速");
                ui.add_enabled(
                    self.config.tts_engine != tts::Engine::Google,
                    egui::Slider::new(&mut self.config.tts_rate, tts::RATE_RANGE)
                        .step_by(0.05)
                        .suffix("×"),
                );
            });
            ui.add_space(10.0);
            let sample = (tts::SAMPLE, tts::Part::Translation);
            let label = match self.speaker.status(sample) {
                tts::Status::Active => "停止试听",
                _ => "试听",
            };
            if ui
                .add(secondary_button(label))
                .on_hover_text("用当前设置（无需先保存）朗读一句目标语言示例")
                .clicked()
            {
                let target = free_translate::target_code(
                    &self.config.target_language,
                    TranslationMode::Google,
                )
                .ok();
                let text = tts::sample_text(tts::voice_for(target, "")).to_owned();
                self.toggle_speech(sample, "", &text, ui.ctx());
            }
            if let tts::Status::Failed(error) = self.speaker.status(sample) {
                ui.add(egui::Label::new(RichText::new(error).small().color(palette.danger)).wrap());
            }
        });
    }

    fn refresh_usage(&mut self, ctx: &egui::Context) {
        if self.usage_rx.is_some() {
            return;
        }
        let (access_key, secret_key) = volcengine_credentials(&self.config);
        let (tx, rx) = mpsc::channel();
        self.usage_rx = Some(rx);
        let ctx = ctx.clone();
        thread::spawn(move || {
            let _ = tx.send(volcengine::query_usage(&access_key, &secret_key));
            tray::wake(&ctx);
        });
    }

    fn poll_usage(&mut self, ctx: &egui::Context) {
        let received = self.usage_rx.as_ref().map(|rx| rx.try_recv());
        match received {
            Some(Ok(result)) => {
                self.usage_rx = None;
                self.volcengine_usage = Some(result);
            }
            Some(Err(mpsc::TryRecvError::Disconnected)) => {
                self.usage_rx = None;
                self.volcengine_usage = Some(Err("费用查询意外结束。".into()));
            }
            Some(Err(mpsc::TryRecvError::Empty)) if tray::main_window_visible() => {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
            _ => {}
        }
    }

    /// Free-tier usage this month and the account balance, queried once when first shown.
    fn draw_volcengine_usage(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        if self.volcengine_usage.is_none() && self.usage_rx.is_none() {
            self.refresh_usage(ui.ctx());
        }
        egui::Frame::new()
            .fill(palette.input)
            .corner_radius(8u8)
            .inner_margin(12i8)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new("本月免费额度").strong().color(palette.text));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if self.usage_rx.is_some() {
                            ui.add(egui::Spinner::new().size(12.0).color(palette.accent));
                        } else if ui
                            .add(
                                egui::Button::new(RichText::new("刷新").color(palette.accent))
                                    .frame(false),
                            )
                            .clicked()
                        {
                            self.refresh_usage(ui.ctx());
                        }
                    });
                });
                match &self.volcengine_usage {
                    None => hint(ui, palette, "正在查询…"),
                    Some(Err(error)) => {
                        ui.label(RichText::new(error).small().color(palette.danger));
                        hint(ui, palette, "查询用量需要该 AK 拥有费用中心的读取权限。");
                    }
                    Some(Ok(usage)) => {
                        let total = volcengine::FREE_CHARS_PER_MONTH;
                        let fraction = (usage.used_chars / total).clamp(0.0, 1.0) as f32;
                        let remaining = (total - usage.used_chars).max(0.0);
                        ui.label(
                            RichText::new(format!(
                                "已用 {} / {} 字符，剩余 {}",
                                format_chars(usage.used_chars),
                                format_chars(total),
                                format_chars(remaining)
                            ))
                            .color(palette.text),
                        );
                        let color = if fraction >= 0.9 {
                            palette.danger
                        } else {
                            palette.accent
                        };
                        // Drawn by hand: the built-in bar's track matches this panel's fill.
                        let (track, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 6.0),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(track, 3.0, palette.border);
                        let mut filled = track;
                        filled.set_width((track.width() * fraction).max(6.0));
                        ui.painter().rect_filled(filled, 3.0, color);
                        if usage.used_chars > total {
                            ui.label(
                                RichText::new("免费额度已用完，超出部分按量从余额扣费。")
                                    .small()
                                    .color(palette.danger),
                            );
                        }
                        let currency = if usage.currency == "CNY" {
                            "元"
                        } else {
                            usage.currency.as_str()
                        };
                        ui.label(
                            RichText::new(format!("账户可用余额：{} {currency}", usage.balance))
                                .color(palette.text),
                        );
                        hint(
                            ui,
                            palette,
                            "数据来自火山账单，可能延迟数小时；按整个账号统计。",
                        );
                    }
                }
            });
    }

    fn draw_proxy_card(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        card(ui, palette, |ui| {
            section_title(ui, palette, "网络代理", "翻译请求使用的代理，保存后生效。");
            segmented(
                ui,
                palette,
                &mut self.config.proxy_mode,
                &[
                    (net::ProxyMode::System, "跟随系统"),
                    (net::ProxyMode::None, "不使用"),
                    (net::ProxyMode::Custom, "自定义"),
                ],
            );
            ui.add_space(4.0);
            match self.config.proxy_mode {
                net::ProxyMode::System => hint(
                    ui,
                    palette,
                    "使用 Windows 设置中的代理；Clash 等工具开启“系统代理”后会自动生效。",
                ),
                net::ProxyMode::None => hint(ui, palette, "所有翻译请求直接连接，不经过代理。"),
                net::ProxyMode::Custom => {
                    ui.add_space(6.0);
                    field_label(ui, palette, "代理地址");
                    ui.add(text_field(&mut self.config.proxy_url).desired_width(f32::INFINITY));
                    match net::validate_proxy_url(&self.config.proxy_url) {
                        Ok(()) => hint(
                            ui,
                            palette,
                            "支持 http:// 和 socks5://，例如 http://127.0.0.1:7890。",
                        ),
                        Err(error) => {
                            ui.label(RichText::new(error).small().color(palette.danger));
                        }
                    }
                }
            }
        });
    }

    fn draw_shortcut_card(&mut self, ui: &mut egui::Ui, palette: &Palette) {
        card(ui, palette, |ui| {
            section_title(
                ui,
                palette,
                "全局快捷键",
                "在任何窗口按下即可框选屏幕并翻译。",
            );
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.shortcut_draft.ctrl, "Ctrl");
                ui.checkbox(&mut self.shortcut_draft.alt, "Alt");
                ui.checkbox(&mut self.shortcut_draft.shift, "Shift");
                ui.add_space(4.0);
                egui::ComboBox::from_id_salt("shortcut_key")
                    .width(72.0)
                    .selected_text(screen_clip::Hotkey::key_name(self.shortcut_draft.key))
                    .show_ui(ui, |ui| {
                        for key in screen_clip::Hotkey::key_choices() {
                            ui.selectable_value(
                                &mut self.shortcut_draft.key,
                                key,
                                screen_clip::Hotkey::key_name(key),
                            );
                        }
                    });
            });
            ui.add_space(4.0);
            match (self.shortcut_draft.validate(), self.active_hotkey) {
                (Err(error), _) => hint(ui, palette, &error),
                (Ok(()), Some(active)) if active != self.shortcut_draft => hint(
                    ui,
                    palette,
                    &format!(
                        "当前生效：{active}。点击“保存设置”后改为 {}。",
                        self.shortcut_draft
                    ),
                ),
                (Ok(()), Some(active)) => hint(ui, palette, &format!("当前生效：{active}")),
                (Ok(()), None) => hint(
                    ui,
                    palette,
                    "当前没有生效的全局快捷键，点击“保存设置”重试注册。",
                ),
            }
        });
    }

    fn draw_result(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let palette = Palette::of(self.config.theme);
        let idle = !self.busy();
        let capture_idle = !self.capture_busy();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(capture_idle, primary_button(&palette, "框选屏幕"))
                .on_hover_text(format!("快捷键：{}", self.shortcut_label()))
                .clicked()
            {
                self.start_screen_capture(ctx);
            }
            if ui.add_enabled(idle, secondary_button("选择图片")).clicked() {
                self.choose_image(ctx);
            }
            if ui
                .add_enabled(idle, secondary_button("翻译原文"))
                .on_hover_text("翻译原文框中输入或修改过的文字")
                .clicked()
            {
                self.start_text_translation(ctx);
            }
            if ui
                .add_enabled(
                    idle && self.selected_file.is_some(),
                    secondary_button("重新翻译"),
                )
                .on_hover_text("对当前图片重新识别并翻译")
                .clicked()
            {
                if let Some(path) = self.selected_file.clone() {
                    self.start_translation(path, ctx);
                }
            }
            if let Some(path) = &self.selected_file {
                let name = if self.temporary_capture.as_ref() == Some(path) {
                    "截图"
                } else {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("已选择图片")
                };
                ui.add_space(6.0);
                ui.label(RichText::new(format!("当前图片：{name}")).color(palette.muted));
            }
        });
        ui.add_space(14.0);
        if let Some(entry) = self.result.dictionary.clone() {
            card(ui, &palette, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("dictionary")
                    .max_height(220.0)
                    .show(ui, |ui| draw_dictionary(ui, &palette, &entry));
            });
            ui.add_space(14.0);
        }

        let paste_released = ui.input(|input| {
            input.events.iter().any(|event| {
                matches!(
                    event,
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: false,
                        modifiers,
                        ..
                    } if modifiers.ctrl
                )
            })
        });
        let paste_keys_down = screen_clip::paste_keys_down();
        let source_hint = format!(
            "输入或粘贴文字，Ctrl+V 可直接粘贴图片\n也可以按 {} 框选屏幕",
            self.shortcut_label()
        );
        let mut source_focused = false;
        let mut source_action = None;
        let mut translation_action = None;
        let speak_label = |part| match self.speaker.status((tts::MAIN, part)) {
            tts::Status::Active => "停止朗读",
            _ => "朗读",
        };
        let speak_source = speak_label(tts::Part::Source);
        let speak_translation = speak_label(tts::Part::Translation);
        ui.columns(2, |columns| {
            (source_focused, source_action) = text_card(
                &mut columns[0],
                &palette,
                "原文",
                &mut self.result.source_text,
                &source_hint,
                &["清空", speak_source],
            );
            (_, translation_action) = text_card(
                &mut columns[1],
                &palette,
                "译文",
                &mut self.result.translated_text,
                "翻译结果会显示在这里",
                &["复制", speak_translation],
            );
        });
        match source_action {
            Some(0) => self.result.source_text.clear(),
            Some(_) => {
                let source = self.result.source_text.clone();
                self.toggle_speech((tts::MAIN, tts::Part::Source), &source, "", ctx);
            }
            None => {}
        }
        match translation_action {
            Some(0) => {
                ctx.copy_text(self.result.translated_text.clone());
                self.status = "译文已复制到剪贴板。".into();
            }
            Some(_) => {
                let source = self.result.source_text.clone();
                let translation = self.result.translated_text.clone();
                let key = (tts::MAIN, tts::Part::Translation);
                self.toggle_speech(key, &source, &translation, ctx);
            }
            None => {}
        }
        if source_focused && ((paste_keys_down || paste_released) && !self.paste_key_down) {
            self.paste_image(ctx);
        }
        self.paste_key_down = paste_keys_down;
    }

    fn show_popups(&mut self, ctx: &egui::Context) {
        if self.popups.is_empty() {
            return;
        }
        let mut popups = std::mem::take(&mut self.popups);
        // One click that lands in any popup window keeps all of them open.
        let [x, y] = screen_clip::cursor_position();
        let cursor = egui::pos2(x as f32, y as f32);
        let click_inside_any = popups
            .iter()
            .any(|popup| popup.placed.is_some_and(|rect| rect.contains(cursor)));
        for index in 0..popups.len() {
            let occupied: Vec<_> = popups
                .iter()
                .enumerate()
                .filter(|(other_index, _)| *other_index != index)
                .filter_map(|(_, popup)| popup.placed)
                .collect();
            self.show_one_popup(ctx, &mut popups[index], click_inside_any, &occupied);
        }
        popups.retain(|popup| !popup.close_requested);
        if popups.iter().any(|popup| !popup.shown) {
            // A hidden popup appears on the next frame; do not wait for the polling interval.
            ctx.request_repaint();
        } else if popups.iter().any(|popup| !popup.pinned) {
            // Unpinned popups poll for dismissal; pinned ones repaint on input events.
            ctx.request_repaint_after(Duration::from_millis(30));
        }
        self.popups = popups;
    }

    fn show_one_popup(
        &mut self,
        ctx: &egui::Context,
        popup: &mut ResultPopup,
        click_inside_any: bool,
        occupied: &[egui::Rect],
    ) {
        if popup.dismissed(click_inside_any) {
            // Not showing the viewport this frame closes its window.
            popup.close_requested = true;
            ctx.request_repaint();
            return;
        }
        let palette = Palette::of(self.config.theme);
        let vision_mode = self.config.mode == TranslationMode::Vision;
        let root_scale = ctx.pixels_per_point();
        let page = popup.result.layout.clone().filter(|_| !popup.pending);
        if let Some(page) = &page {
            // Vision captures open as images; OCR prose retains its compact text default.
            popup
                .image_view
                .get_or_insert(vision_mode || page.dispersed);
        }
        let image_view = page.is_some() && popup.image_view == Some(true);
        // Show the capture at its real size, shrunk to fit two thirds of the monitor —
        // or, once the user resized the window, shrunk to fit the window itself.
        let [left, top, right, bottom] = popup.work_area.map(|value| value as f32 / root_scale);
        let window_points = popup.placed.map(|rect| rect.size() / root_scale);
        let image_size = page.as_ref().map(|page| {
            let natural = egui::vec2(page.width as f32, page.height as f32) / root_scale;
            if popup.user_sized {
                let avail = egui::vec2(
                    (popup.width - 2.0 * f32::from(POPUP_MARGIN)).max(60.0),
                    (window_points.map_or(240.0, |size| size.y)
                        - 2.0 * f32::from(POPUP_MARGIN)
                        - 70.0)
                        .max(60.0),
                );
                natural * (avail.x / natural.x).min(avail.y / natural.y)
            } else {
                let limit = egui::vec2((right - left) * 0.66, (bottom - top) * 0.75 - 80.0);
                natural * (limit.x / natural.x).min(limit.y / natural.y).min(1.0)
            }
        });
        if !popup.user_sized {
            popup.width = match image_size {
                Some(size) if image_view => {
                    (size.x + 2.0 * f32::from(POPUP_MARGIN)).max(POPUP_WIDTH)
                }
                _ => POPUP_WIDTH,
            };
        }
        let width = popup.width;
        let first_placement = popup.placed.is_none();
        let initial = popup.place_avoiding(egui::vec2(width, popup.height) * root_scale, occupied);
        let mut builder = egui::ViewportBuilder::default()
            .with_title("OCR Translate 译文")
            .with_decorations(false)
            .with_resizable(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_active(false);
        if first_placement {
            // Windows shows a new window at its default size and place before it takes the
            // requested ones, so the popup starts hidden instead of flashing there.
            builder = builder
                .with_visible(false)
                .with_inner_size([width, popup.height])
                .with_position([initial.x / root_scale, initial.y / root_scale]);
        }
        let id = egui::ViewportId::from_hash_of(egui::Id::new(("result_popup", popup.id)));
        let speech = PopupSpeech {
            translation: self.speaker.status((popup.id, tts::Part::Translation)),
            source: self.speaker.status((popup.id, tts::Part::Source)),
        };
        // With two-way translation the direction depends on the text, so say which it took.
        let target = self
            .config
            .two_way
            .then(|| target_for(&self.config, &popup.result.source_text))
            .flatten()
            .map(str::to_owned);
        let target = target.as_deref();
        let mut actions = PopupActions::default();
        ctx.show_viewport_immediate(id, builder, |ctx, _| {
            let scale = ctx.pixels_per_point();
            let [left, top, right, bottom] = popup.work_area.map(|value| value as f32 / scale);
            let window_points = popup.placed.map(|rect| rect.size() / scale);
            let image_size = page.as_ref().map(|page| {
                let natural = egui::vec2(page.width as f32, page.height as f32) / scale;
                if popup.user_sized {
                    let avail = egui::vec2(
                        (popup.width - 2.0 * f32::from(POPUP_MARGIN)).max(60.0),
                        (window_points.map_or(240.0, |size| size.y)
                            - 2.0 * f32::from(POPUP_MARGIN)
                            - 70.0)
                            .max(60.0),
                    );
                    natural * (avail.x / natural.x).min(avail.y / natural.y)
                } else {
                    let limit = egui::vec2((right - left) * 0.66, (bottom - top) * 0.75 - 80.0);
                    natural * (limit.x / natural.x).min(limit.y / natural.y).min(1.0)
                }
            });
            if !popup.user_sized {
                popup.width = match image_size {
                    Some(size) if image_view => {
                        (size.x + 2.0 * f32::from(POPUP_MARGIN)).max(POPUP_WIDTH)
                    }
                    _ => POPUP_WIDTH,
                };
            }
            let width = popup.width;
            let max_height = if image_view {
                (bottom - top) * 0.9
            } else {
                POPUP_MAX_HEIGHT
            };
            let content_height = egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(palette.surface)
                        .stroke(Stroke::new(1.0_f32, palette.border))
                        .inner_margin(POPUP_MARGIN),
                )
                .show(ctx, |ui| {
                    ui.scope(|ui| {
                        popup.draw(ui, &palette, image_size, target, &speech, &mut actions)
                    })
                    .response
                    .rect
                    .height()
                })
                .inner;

            // Fit the window to its content unless the user took over its geometry.
            let height = (content_height + 2.0 * f32::from(POPUP_MARGIN)).clamp(80.0, max_height);
            let size = if popup.user_sized {
                popup
                    .placed
                    .map_or(egui::vec2(width, height) * scale, |rect| rect.size())
            } else {
                egui::vec2(width, height) * scale
            };
            let first_frame = popup.placed.is_none();
            let corner = match popup.placed {
                Some(rect) if popup.moved || popup.shown => popup.settle(rect.min, size),
                _ => popup.place_avoiding(size, occupied),
            };
            let rect = egui::Rect::from_min_size(corner, size);
            let settled = popup.placed == Some(rect);
            if !settled {
                popup.placed = Some(rect);
                popup.width = size.x / scale;
                popup.height = size.y / scale;
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size / scale));
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                    corner.x / scale,
                    corner.y / scale,
                )));
            }
            // The hidden window took its size and place after the first frame; from the
            // second on it is shown, and later content changes grow it where it stands.
            if !popup.shown && !first_frame {
                popup.shown = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            }
            if ctx.input(|input| input.viewport().close_requested()) {
                actions.close = true;
            }
        });
        if let Some(part) = actions.toggle_speech {
            let result = &popup.result;
            self.toggle_speech(
                (popup.id, part),
                &result.source_text,
                &result.translated_text,
                ctx,
            );
        }
        if actions.open_main_window {
            self.lightweight = false;
            let _ = self.client.send(ipc::Command::Open(ipc::View::Main));
            self.settings_open = false;
            self.reveal_main_window(ctx);
            actions.close = true;
        }
        if actions.close {
            popup.close_requested = true;
        }
    }

    fn draw_status_bar(&self, ui: &mut egui::Ui, palette: &Palette) {
        egui::Sides::new().shrink_left().show(
            ui,
            |ui| {
                if self.busy() {
                    ui.add(egui::Spinner::new().size(12.0).color(palette.accent));
                } else {
                    let (dot, _) =
                        ui.allocate_exact_size(egui::vec2(12.0, 12.0), egui::Sense::hover());
                    ui.painter()
                        .circle_filled(dot.center(), 3.5, palette.accent);
                }
                ui.add(
                    egui::Label::new(RichText::new(&self.status).color(palette.text)).truncate(),
                );
            },
            |ui| {
                let shortcut = self
                    .active_hotkey
                    .map_or_else(|| "快捷键未注册".into(), |hotkey| hotkey.to_string());
                ui.label(
                    RichText::new(format!("{}  ·  {shortcut}", self.config.mode.short_label()))
                        .color(palette.muted),
                );
            },
        );
    }
}

impl eframe::App for OcrTranslateApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_backend(ctx);
        if ctx.input(|input| input.viewport().close_requested())
            && !self.exit_requested
            && self.tray_available
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.apply_close_behavior(self.config.close_behavior, ctx);
        }
        if self.exit_requested {
            return;
        }
        self.poll_usage(ctx);
        if let Some(((window, _), error)) = self.speaker.poll() {
            if window == tts::MAIN || window == tts::SAMPLE {
                self.status = error;
            }
        }
        if !self.main_visible || !tray::main_window_visible() {
            // Workers and native tray/hotkey events wake the root window when needed.
            self.show_popups(ctx);
            self.finish_frame(ctx);
            return;
        }
        let palette = Palette::of(self.config.theme);

        egui::TopBottomPanel::top("top_bar")
            .frame(
                egui::Frame::new()
                    .fill(palette.surface)
                    .inner_margin(egui::Margin::symmetric(20, 12)),
            )
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("OCR Translate")
                            .heading()
                            .strong()
                            .color(palette.text),
                    );
                    ui.add_space(6.0);
                    ui.label(RichText::new("截图识别 · 即时翻译").color(palette.muted));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        segmented(
                            ui,
                            &palette,
                            &mut self.settings_open,
                            &[(false, "翻译"), (true, "设置")],
                        );
                    });
                });
            });

        egui::TopBottomPanel::bottom("status_bar")
            .frame(
                egui::Frame::new()
                    .fill(palette.surface)
                    .inner_margin(egui::Margin::symmetric(20, 8)),
            )
            .show(ctx, |ui| self.draw_status_bar(ui, &palette));

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(palette.background)
                    .inner_margin(egui::Margin::same(20)),
            )
            .show(ctx, |ui| {
                if self.settings_open {
                    egui::ScrollArea::vertical()
                        .auto_shrink(false)
                        .show(ui, |ui| self.draw_settings(ui));
                } else {
                    self.draw_result(ui, ctx);
                }
            });
        self.show_popups(ctx);
        self.finish_frame(ctx);
    }
}

fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("ocr-translate")
        .join(CONFIG_FILE)
}

fn load_config(path: &Path) -> AppConfig {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn volcengine_credentials(config: &AppConfig) -> (String, String) {
    let access_key = if config.volcengine_access_key.trim().is_empty() {
        std::env::var("VOLCENGINE_ACCESS_KEY").unwrap_or_default()
    } else {
        config.volcengine_access_key.clone()
    };
    let secret_key = if config.volcengine_secret_key.trim().is_empty() {
        std::env::var("VOLCENGINE_SECRET_KEY").unwrap_or_default()
    } else {
        config.volcengine_secret_key.clone()
    };
    (access_key, secret_key)
}

/// The language to translate `source` into. With two-way translation, text written in the
/// target language's script goes into the second language and anything else into the target
/// language. `None` when that cannot be told: the text has no letters, or a vision model's
/// free-form target language is not one whose script is known here.
fn target_for<'a>(config: &'a AppConfig, source: &str) -> Option<&'a str> {
    if !config.two_way {
        return Some(&config.target_language);
    }
    let target =
        free_translate::target_code(&config.target_language, TranslationMode::Google).ok()?;
    let script = language::dominant(source)?;
    Some(if script == language::Script::of(target) {
        &config.second_language
    } else {
        &config.target_language
    })
}

/// The settings for translating `source`, its target language settled. Two-way translation
/// stays on only while the direction is open, for a vision model to settle from the image.
fn directed(config: &AppConfig, source: &str) -> AppConfig {
    let mut directed = config.clone();
    if let Some(target) = target_for(config, source) {
        directed.target_language = target.to_owned();
        directed.two_way = false;
    }
    directed
}

/// The target language as a vision model prompt puts it, leaving an open direction to the model.
fn vision_target(config: &AppConfig) -> String {
    if config.two_way {
        format!(
            "{0}（如果原文本身就是{0}，则翻译成{1}）",
            config.target_language, config.second_language
        )
    } else {
        config.target_language.clone()
    }
}

/// Connects to the server a capture will be translated by while the user is still selecting.
fn warm_up_translation(config: &AppConfig) {
    let url = match config.mode {
        TranslationMode::Tencent => free_translate::TENCENT_URL.to_owned(),
        TranslationMode::Google => free_translate::GOOGLE_URL.to_owned(),
        TranslationMode::Volcengine => format!("https://{}/", volcengine::TRANSLATE_HOST),
        TranslationMode::Vision => config.api_url.trim().to_owned(),
    };
    if url.starts_with("https://") || url.starts_with("http://") {
        net::warm_up(url);
    }
}

/// Translates each text separately, returning one translation per input in order. All of them
/// go in the one direction their combined text calls for.
fn translate_many_cancellable(
    config: &AppConfig,
    texts: &[String],
    cancelled: &AtomicBool,
) -> Result<Vec<String>, String> {
    let config = &directed(config, &texts.join("\n"));
    let check_cancelled = || {
        if cancelled.load(Ordering::Acquire) {
            Err("任务已被新的请求替换。".to_string())
        } else {
            Ok(())
        }
    };
    check_cancelled()?;
    match config.mode {
        TranslationMode::Volcengine => {
            let target = free_translate::target_code(&config.target_language, config.mode)?;
            let (access_key, secret_key) = volcengine_credentials(config);
            let result = volcengine::translate_list(texts, target, &access_key, &secret_key)?;
            check_cancelled()?;
            Ok(result)
        }
        TranslationMode::Vision => {
            let mut translations = Vec::with_capacity(texts.len());
            for text in texts {
                check_cancelled()?;
                translations.push(translate_text(config, text)?);
            }
            Ok(translations)
        }
        TranslationMode::Tencent | TranslationMode::Google => {
            let mut translations = Vec::with_capacity(texts.len());
            let mut start = 0;
            while start < texts.len() {
                check_cancelled()?;
                let mut end = start + 1;
                let mut length = texts[start].len();
                while end < texts.len() && length + texts[end].len() < 1500 {
                    length += texts[end].len() + 1;
                    end += 1;
                }
                translations.extend(translate_lines(config, &texts[start..end])?);
                check_cancelled()?;
                start = end;
            }
            Ok(translations)
        }
    }
}

/// Joins texts with line breaks in one request. If the provider merges or splits lines, falls
/// back to translating each text on its own, a few at a time.
fn translate_lines(config: &AppConfig, texts: &[String]) -> Result<Vec<String>, String> {
    if texts.len() > 1 {
        let joined =
            free_translate::translate(&texts.join("\n"), &config.target_language, config.mode)?;
        let lines: Vec<String> = joined
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned)
            .collect();
        if lines.len() == texts.len() {
            return Ok(lines);
        }
    }
    let mut translations = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(6) {
        let results: Vec<Result<String, String>> = thread::scope(|scope| {
            let handles: Vec<_> = chunk
                .iter()
                .map(|text| scope.spawn(|| translate_text(config, text)))
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle
                        .join()
                        .unwrap_or_else(|_| Err("翻译任务意外结束。".into()))
                })
                .collect()
        });
        for result in results {
            translations.push(result?);
        }
    }
    Ok(translations)
}

/// Adds dictionary details for a single English word translated into Chinese. A failed lookup
/// leaves the translation as it is.
fn with_dictionary(config: &AppConfig, mut result: TranslationResult) -> TranslationResult {
    let target = target_for(config, &result.source_text).unwrap_or(&config.target_language);
    if target.contains('中') && dictionary::is_single_english_word(&result.source_text) {
        result.dictionary = dictionary::lookup(&result.source_text).map(Arc::new);
    }
    result
}

fn translate_text(config: &AppConfig, source: &str) -> Result<String, String> {
    let config = &directed(config, source);
    match config.mode {
        TranslationMode::Vision => translate_text_with_vision(config, source),
        TranslationMode::Volcengine => {
            let target = free_translate::target_code(&config.target_language, config.mode)?;
            let (access_key, secret_key) = volcengine_credentials(config);
            volcengine::translate(source, target, &access_key, &secret_key)
        }
        TranslationMode::Tencent | TranslationMode::Google => {
            free_translate::translate(source, &config.target_language, config.mode)
        }
    }
}

#[cfg(test)]
fn translate_image(config: &AppConfig, path: &Path) -> Result<TranslationResult, String> {
    translate_image_with_progress(config, path, |_| {})
}

fn translate_image_with_progress(
    config: &AppConfig,
    path: &Path,
    on_source: impl Fn(&str),
) -> Result<TranslationResult, String> {
    validate_image_size(path)?;
    // Position hints improve placement, but vision translation also works without an OCR pack.
    let lines = windows_ocr::recognize_lines(path, &config.ocr_language).unwrap_or_default();
    let page = layout::ImageLayout::build(path, lines)?;
    let source = page.source_text();
    on_source(&source);
    let config = &directed(config, &source);
    let references: Vec<Value> = page
        .translatable()
        .into_iter()
        .map(|id| {
            let block = &page.blocks[id];
            serde_json::json!({"id": id, "source_text": block.source})
        })
        .collect();
    let bytes = fs::read(path).map_err(|error| format!("读取图片失败：{error}"))?;
    let mime = match path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    };
    let image_data = format!("data:{mime};base64,{}", STANDARD.encode(bytes));
    let prompt = format!(
        "识别图片中的所有可读文字，并翻译成{}。只返回严格 JSON，不要 Markdown，不要额外说明。\n\
         格式为 {{\"source_text\":\"完整原文\",\"translated_text\":\"完整译文\",\"blocks\":[{{\"id\":0,\"source_text\":\"该块原文\",\"translated_text\":\"该块译文\"}}]}}。\n\
         blocks 按阅读顺序逐块翻译。下面的本地 OCR 参考块只是位置提示，可根据图片纠正识别文字；\
         对每个参考块返回相同的 id 和该块的译文，不要合并不同 id。\n\
         对参考块之外的文字，或参考块为空时，省略 id，返回 bbox:[x,y,width,height] 和 line_count；\
         bbox 是原文文字区域的左上角、宽、高，按整张图片归一化为 0 到 1000（不是右下角坐标），\
         line_count 是该区域的原文行数。不要给同一文字重复返回多个块。\n\
         保留完整原文和译文的换行。数字、价格等无需翻译的内容保持不变。\n本地 OCR 参考块：{}",
        vision_target(config),
        serde_json::to_string(&references).map_err(|error| error.to_string())?
    );
    let response = request_chat_completion(
        config,
        serde_json::json!([
            {"type": "text", "text": prompt},
            {"type": "image_url", "image_url": {"url": image_data}}
        ]),
    )?;
    parse_image_translation_response(&response, page)
}

fn translate_text_with_vision(config: &AppConfig, source: &str) -> Result<String, String> {
    let prompt = format!(
        "将下面的原文翻译成{}。只返回严格 JSON，不要 Markdown，不要额外说明，格式为 {{\"source_text\":\"原文\",\"translated_text\":\"译文\"}}。原文如下：\n{}",
        vision_target(config),
        source
    );
    Ok(chat_completion(config, Value::String(prompt))?.translated_text)
}

/// Sends one user message to the OpenAI-compatible endpoint and parses the JSON reply.
fn chat_completion(config: &AppConfig, content: Value) -> Result<TranslationResult, String> {
    parse_translation_response(&request_chat_completion(config, content)?)
}

fn request_chat_completion(config: &AppConfig, content: Value) -> Result<String, String> {
    let body = serde_json::json!({
        "model": config.model,
        "temperature": 0,
        "messages": [{
            "role": "user",
            "content": content
        }]
    });
    let response = net::client()?
        .post(&config.api_url)
        .timeout(Duration::from_secs(120))
        .bearer_auth(&config.api_key)
        .json(&body)
        .send()
        .map_err(|error| format!("请求失败：{error}"))?;
    let status = response.status();
    let text = response
        .text()
        .map_err(|error| format!("读取响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("接口返回 {}：{}", status, compact_error(&text)));
    }
    Ok(text)
}

fn validate_image_size(path: &Path) -> Result<(), String> {
    let metadata = fs::metadata(path).map_err(|error| format!("读取图片信息失败：{error}"))?;
    if metadata.len() > MAX_IMAGE_BYTES {
        return Err("图片超过 20 MB，请先裁剪或压缩后重试。".into());
    }
    Ok(())
}

fn parse_translation_response(text: &str) -> Result<TranslationResult, String> {
    translation_from_value(&parse_model_response(text)?)
}

fn parse_image_translation_response(
    text: &str,
    mut page: layout::ImageLayout,
) -> Result<TranslationResult, String> {
    let mut parsed = parse_model_response(text)?;
    let blocks = parsed["blocks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| serde_json::from_value::<layout::VisionBlock>(block.clone()).ok());
    page.apply_vision_blocks(blocks);
    if parsed["source_text"]
        .as_str()
        .unwrap_or("")
        .trim()
        .is_empty()
        && parsed["original"].as_str().unwrap_or("").trim().is_empty()
    {
        parsed["source_text"] = Value::String(page.source_text());
    }
    if parsed["translated_text"]
        .as_str()
        .unwrap_or("")
        .trim()
        .is_empty()
        && parsed["translation"]
            .as_str()
            .unwrap_or("")
            .trim()
            .is_empty()
        && page.blocks.iter().any(|block| block.translated.is_some())
    {
        parsed["translated_text"] = Value::String(page.translated_text());
    }
    let mut result = translation_from_value(&parsed)?;
    result.layout = Some(Arc::new(page));
    Ok(result)
}

fn parse_model_response(text: &str) -> Result<Value, String> {
    let root: Value =
        serde_json::from_str(text).map_err(|error| format!("接口响应不是有效 JSON：{error}"))?;
    let content = root["choices"][0]["message"]["content"]
        .as_str()
        .ok_or_else(|| "接口响应中没有找到模型文本。".to_string())?;
    let clean = content
        .trim()
        .trim_start_matches("```json")
        .trim_start_matches("```")
        .trim_end_matches("```")
        .trim();
    let parsed: Value = serde_json::from_str(clean)
        .or_else(|_| {
            let start = clean.find('{').ok_or(())?;
            let end = clean.rfind('}').ok_or(())?;
            serde_json::from_str(&clean[start..=end]).map_err(|_| ())
        })
        .map_err(|_| "模型返回的内容无法解析为原文和译文。".to_string())?;
    if !parsed.is_object() {
        return Err("模型返回的内容无法解析为原文和译文。".into());
    }
    Ok(parsed)
}

fn translation_from_value(parsed: &Value) -> Result<TranslationResult, String> {
    let source = parsed["source_text"]
        .as_str()
        .or_else(|| parsed["original"].as_str())
        .unwrap_or("");
    let translated = parsed["translated_text"]
        .as_str()
        .or_else(|| parsed["translation"].as_str())
        .unwrap_or("");
    if source.is_empty() && translated.is_empty() {
        return Err("模型返回了空的原文和译文。".into());
    }
    Ok(TranslationResult {
        source_text: source.into(),
        translated_text: translated.into(),
        layout: None,
        dictionary: None,
    })
}

fn compact_error(text: &str) -> String {
    text.replace(['\r', '\n'], " ").chars().take(300).collect()
}

/// Colours shared by the custom widgets and the egui visuals for one theme.
struct Palette {
    background: Color32,
    surface: Color32,
    input: Color32,
    hover: Color32,
    pressed: Color32,
    border: Color32,
    text: Color32,
    muted: Color32,
    danger: Color32,
    accent: Color32,
    on_accent: Color32,
}

impl Palette {
    fn of(theme: ThemeMode) -> Self {
        match theme {
            ThemeMode::Day => Self {
                background: Color32::from_rgb(0xF3, 0xF4, 0xF7),
                surface: Color32::from_rgb(0xFF, 0xFF, 0xFF),
                input: Color32::from_rgb(0xF6, 0xF7, 0xF9),
                hover: Color32::from_rgb(0xEC, 0xEE, 0xF2),
                pressed: Color32::from_rgb(0xE2, 0xE5, 0xEB),
                border: Color32::from_rgb(0xE1, 0xE4, 0xEA),
                text: Color32::from_rgb(0x1F, 0x23, 0x29),
                muted: Color32::from_rgb(0x6B, 0x72, 0x80),
                danger: Color32::from_rgb(0xD9, 0x3F, 0x3F),
                accent: Color32::from_rgb(0x3B, 0x6E, 0xF5),
                on_accent: Color32::WHITE,
            },
            ThemeMode::Night => Self {
                background: Color32::from_rgb(0x14, 0x16, 0x1B),
                surface: Color32::from_rgb(0x1C, 0x1F, 0x26),
                input: Color32::from_rgb(0x23, 0x27, 0x2F),
                hover: Color32::from_rgb(0x2B, 0x30, 0x3A),
                pressed: Color32::from_rgb(0x33, 0x39, 0x45),
                border: Color32::from_rgb(0x2D, 0x32, 0x3C),
                text: Color32::from_rgb(0xE6, 0xE8, 0xEC),
                muted: Color32::from_rgb(0x8B, 0x93, 0xA1),
                danger: Color32::from_rgb(0xF2, 0x6D, 0x6D),
                accent: Color32::from_rgb(0x5B, 0x8C, 0xFF),
                on_accent: Color32::WHITE,
            },
        }
    }
}

fn apply_appearance(ctx: &egui::Context, config: &AppConfig) {
    let palette = Palette::of(config.theme);
    let mut visuals = match config.theme {
        ThemeMode::Day => egui::Visuals::light(),
        ThemeMode::Night => egui::Visuals::dark(),
    };
    visuals.panel_fill = palette.background;
    visuals.window_fill = palette.surface;
    visuals.window_stroke = Stroke::new(1.0_f32, palette.border);
    visuals.extreme_bg_color = palette.input;
    visuals.text_edit_bg_color = Some(palette.input);
    visuals.faint_bg_color = palette.input;
    visuals.hyperlink_color = palette.accent;
    visuals.selection.bg_fill = palette.accent.gamma_multiply(0.45);
    visuals.selection.stroke = Stroke::new(1.0_f32, palette.accent);
    visuals.window_corner_radius = egui::CornerRadius::same(12);
    visuals.menu_corner_radius = egui::CornerRadius::same(8);
    let shadow = egui::Shadow {
        offset: [0, 6],
        blur: 18,
        spread: 0,
        color: Color32::from_black_alpha(match config.theme {
            ThemeMode::Day => 30,
            ThemeMode::Night => 90,
        }),
    };
    visuals.window_shadow = shadow;
    visuals.popup_shadow = shadow;

    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = palette.surface;
    widgets.noninteractive.weak_bg_fill = palette.surface;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, palette.border);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, palette.text);
    widgets.noninteractive.corner_radius = egui::CornerRadius::same(8);
    for (state, fill, stroke) in [
        (&mut widgets.inactive, palette.input, palette.border),
        (
            &mut widgets.hovered,
            palette.hover,
            palette.accent.gamma_multiply(0.6),
        ),
        (&mut widgets.active, palette.pressed, palette.accent),
        (&mut widgets.open, palette.input, palette.border),
    ] {
        state.bg_fill = fill;
        state.weak_bg_fill = fill;
        state.bg_stroke = Stroke::new(1.0_f32, stroke);
        state.fg_stroke = Stroke::new(1.0_f32, palette.text);
        state.corner_radius = egui::CornerRadius::same(8);
        state.expansion = 0.0;
    }
    ctx.set_visuals(visuals);

    let size = config
        .font_size
        .clamp(*FONT_SIZE_RANGE.start(), *FONT_SIZE_RANGE.end()) as f32;
    let mut style = (*ctx.style()).clone();
    for (text_style, font_size) in [
        (egui::TextStyle::Body, size),
        (egui::TextStyle::Button, size),
        (egui::TextStyle::Monospace, size),
        (egui::TextStyle::Heading, size + 5.0),
        (egui::TextStyle::Small, (size - 2.0).max(10.0)),
    ] {
        if let Some(font) = style.text_styles.get_mut(&text_style) {
            font.size = font_size;
        }
    }
    style.spacing.item_spacing = egui::vec2(8.0, 8.0);
    style.spacing.button_padding = egui::vec2(14.0, 6.0);
    style.spacing.interact_size.y = size + 16.0;
    style.spacing.slider_width = 260.0;
    style.visuals.slider_trailing_fill = true;
    ctx.set_style(style);
    ctx.request_repaint();
}

/// Draws the capture and, unless `original` is set, paints each translated block over its
/// source text in the sampled colours. Hovering a block shows its source text.
fn paint_translated_capture(
    ui: &mut egui::Ui,
    texture: &egui::TextureHandle,
    page: &layout::ImageLayout,
    size: egui::Vec2,
    original: bool,
) {
    const MIN_FONT: f32 = 7.0;
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    let painter = ui.painter_at(rect);
    let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
    painter.image(texture.id(), rect, uv, Color32::WHITE);
    let scale = size.x / page.width as f32;
    let mut hovered = None;
    for block in &page.blocks {
        let [x, y, width, height] = block.rect;
        let area = egui::Rect::from_min_size(
            rect.min + egui::vec2(x, y) * scale,
            egui::vec2(width, height) * scale,
        );
        if response
            .hover_pos()
            .is_some_and(|pointer| area.contains(pointer))
        {
            hovered = Some(block.source.clone());
        }
        let Some(text) = block.translated.as_ref().filter(|_| !original) else {
            continue;
        };
        let [r, g, b] = block.background;
        painter.rect_filled(area.expand(2.0), 2.0, Color32::from_rgb(r, g, b));
        let [r, g, b] = block.foreground;
        let color = Color32::from_rgb(r, g, b);
        // Start near the source font size and shrink until the text fits the source box.
        let mut font_size = (block.line_height * scale * 0.95).max(MIN_FONT);
        let galley = loop {
            let galley = painter.layout(
                text.clone(),
                egui::FontId::proportional(font_size),
                color,
                area.width().max(1.0),
            );
            if galley.size().y <= area.height() * 1.15 || font_size <= MIN_FONT {
                break galley;
            }
            font_size = (font_size * 0.9).max(MIN_FONT);
        };
        let top = area.center().y - galley.size().y / 2.0;
        painter.galley(egui::pos2(area.left(), top), galley, color);
    }
    if let Some(source) = hovered {
        response.on_hover_text(source);
    }
}

fn draw_dictionary(ui: &mut egui::Ui, palette: &Palette, entry: &dictionary::Entry) {
    ui.horizontal_wrapped(|ui| {
        let size = ui.style().text_styles[&egui::TextStyle::Body].size + 4.0;
        ui.label(
            RichText::new(&entry.word)
                .size(size)
                .strong()
                .color(palette.text),
        );
        for (label, phonetic) in [("英", &entry.uk_phonetic), ("美", &entry.us_phonetic)] {
            if let Some(phonetic) = phonetic {
                ui.label(RichText::new(format!("{label} /{phonetic}/")).color(palette.muted));
            }
        }
    });
    for (pos, meaning) in &entry.senses {
        ui.horizontal_top(|ui| {
            if !pos.is_empty() {
                ui.label(RichText::new(pos).strong().color(palette.accent));
            }
            ui.add(
                egui::Label::new(RichText::new(meaning).color(palette.text))
                    .wrap()
                    .selectable(true),
            );
        });
    }
    if !entry.forms.is_empty() {
        let forms: Vec<String> = entry
            .forms
            .iter()
            .map(|(name, value)| format!("{name} {value}"))
            .collect();
        hint(ui, palette, &forms.join(" · "));
    }
    if !entry.examples.is_empty() {
        egui::CollapsingHeader::new(
            RichText::new(format!("例句（{}）", entry.examples.len())).color(palette.accent),
        )
        .id_salt(("examples", &entry.word))
        .show(ui, |ui| {
            for (english, chinese) in &entry.examples {
                ui.add(
                    egui::Label::new(RichText::new(english).color(palette.text))
                        .wrap()
                        .selectable(true),
                );
                ui.add(egui::Label::new(RichText::new(chinese).color(palette.muted)).wrap());
                ui.add_space(4.0);
            }
        });
    }
}

/// `16207` → "1.6 万", `2000000` → "200 万", small numbers as they are.
fn format_chars(chars: f64) -> String {
    if chars >= 10_000.0 {
        let wan = chars / 10_000.0;
        if (wan - wan.round()).abs() < 0.05 {
            format!("{} 万", wan.round())
        } else {
            format!("{wan:.1} 万")
        }
    } else {
        format!("{}", chars.round())
    }
}

fn card<R>(
    ui: &mut egui::Ui,
    palette: &Palette,
    add_contents: impl FnOnce(&mut egui::Ui) -> R,
) -> egui::InnerResponse<R> {
    egui::Frame::new()
        .fill(palette.surface)
        .stroke(Stroke::new(1.0_f32, palette.border))
        .corner_radius(12u8)
        .inner_margin(18i8)
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add_contents(ui)
        })
}

fn draw_about_card(ui: &mut egui::Ui, palette: &Palette) -> egui::InnerResponse<egui::Response> {
    card(ui, palette, |ui| {
        section_title(ui, palette, "关于", "应用信息与官方入口。");
        ui.horizontal_wrapped(|ui| {
            let size = ui.style().text_styles[&egui::TextStyle::Body].size + 3.0;
            ui.label(
                RichText::new("OCR Translate")
                    .size(size)
                    .strong()
                    .color(palette.text),
            );
            egui::Frame::new()
                .fill(palette.input)
                .corner_radius(6u8)
                .inner_margin(egui::Margin::symmetric(8, 3))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new(format!("v{APP_VERSION}"))
                            .small()
                            .color(palette.muted),
                    );
                });
        });
        hint(ui, palette, "轻量的 Windows 截图 OCR 与翻译工具。");
        ui.add_space(12.0);
        field_label(ui, palette, "官方网站 · GitHub 仓库");
        ui.add(
            egui::Label::new(RichText::new(OFFICIAL_WEBSITE).color(palette.text))
                .wrap()
                .selectable(true),
        );
        ui.add_space(10.0);
        ui.horizontal_wrapped(|ui| {
            let response = ui
                .add(secondary_button("访问官方网站"))
                .on_hover_text("在默认浏览器中打开官方 GitHub 仓库");
            if response.clicked() {
                ui.ctx().open_url(egui::OpenUrl::new_tab(OFFICIAL_WEBSITE));
            }
            hint(ui, palette, "查看项目说明、下载版本与反馈问题。");
            response
        })
        .inner
    })
}

fn section_title(ui: &mut egui::Ui, palette: &Palette, title: &str, subtitle: &str) {
    let size = ui.style().text_styles[&egui::TextStyle::Body].size + 3.0;
    ui.label(RichText::new(title).size(size).strong().color(palette.text));
    hint(ui, palette, subtitle);
    ui.add_space(10.0);
}

fn field_label(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.label(RichText::new(text).color(palette.muted));
}

fn hint(ui: &mut egui::Ui, palette: &Palette, text: &str) {
    ui.label(RichText::new(text).small().color(palette.muted));
}

fn primary_button(palette: &Palette, text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).strong().color(palette.on_accent))
        .fill(palette.accent)
        .stroke(Stroke::NONE)
}

fn text_field(text: &mut String) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).margin(egui::vec2(10.0, 7.0))
}

/// A choice of the supported target languages, or any language typed in for a vision model.
fn language_field(ui: &mut egui::Ui, id: &str, language: &mut String, free_form: bool) {
    if free_form {
        ui.add(text_field(language).desired_width(f32::INFINITY));
        return;
    }
    egui::ComboBox::from_id_salt(id)
        .width(ui.available_width())
        .selected_text(language.as_str())
        .show_ui(ui, |ui| {
            for option in TARGET_LANGUAGES {
                ui.selectable_value(language, option.into(), option);
            }
        });
}

/// Explains which way two-way translation goes with the chosen pair of languages.
fn two_way_hint(config: &AppConfig) -> String {
    let (target, second) = (config.target_language.trim(), config.second_language.trim());
    let script = |name| {
        free_translate::target_code(name, TranslationMode::Google)
            .ok()
            .map(language::Script::of)
    };
    let rule = format!("原文是{target}时译成{second}，其他语言都译成{target}。");
    match (script(target), script(second)) {
        (Some(main), Some(other)) if main == other => format!(
            "{target}和{second}都使用{}，无法按文字区分：这种文字的原文都会译成{second}。",
            main.label()
        ),
        (Some(language::Script::Latin), _) => {
            format!("{rule}按文字判断，英、法、西、德等拉丁字母的原文都会当作{target}。")
        }
        (Some(_), _) => format!("{rule}按原文的文字判断。"),
        (None, _) => format!("{rule}由视觉模型判断原文语言。"),
    }
}

fn secondary_button(text: &str) -> egui::Button<'static> {
    egui::Button::new(text.to_owned())
}

/// A pill-shaped switch between a few options. Returns true when the value changed.
fn segmented<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    palette: &Palette,
    value: &mut T,
    options: &[(T, &str)],
) -> bool {
    let mut changed = false;
    egui::Frame::new()
        .fill(palette.input)
        .corner_radius(9u8)
        .inner_margin(3i8)
        .show(ui, |ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.horizontal(|ui| {
                let mut ordered: Vec<&(T, &str)> = options.iter().collect();
                // In a right-to-left layout, add in reverse so the options still read left to right.
                if ui.layout().prefer_right_to_left() {
                    ordered.reverse();
                }
                for (option, label) in ordered {
                    let selected = *value == *option;
                    let text = RichText::new(*label).color(if selected {
                        palette.on_accent
                    } else {
                        palette.muted
                    });
                    let button = egui::Button::new(text)
                        .fill(if selected {
                            palette.accent
                        } else {
                            Color32::TRANSPARENT
                        })
                        .stroke(Stroke::NONE)
                        .corner_radius(7u8)
                        .min_size(egui::vec2(64.0, 0.0));
                    if ui.add(button).clicked() && !selected {
                        *value = *option;
                        changed = true;
                    }
                }
            });
        });
    changed
}

/// A titled card with a borderless editor filling the rest of the column.
/// Returns whether the editor has focus and which header action, if any, was clicked;
/// the actions are laid out right to left starting with the first.
fn text_card(
    ui: &mut egui::Ui,
    palette: &Palette,
    title: &str,
    text: &mut String,
    hint_text: &str,
    actions: &[&str],
) -> (bool, Option<usize>) {
    let mut action_clicked = None;
    let focused = card(ui, palette, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(title).strong().color(palette.text));
            let count = text.chars().filter(|c| !c.is_whitespace()).count();
            if count > 0 {
                ui.label(
                    RichText::new(format!("{count} 字"))
                        .small()
                        .color(palette.muted),
                );
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                for (index, action) in actions.iter().enumerate() {
                    if ui
                        .add_enabled(
                            !text.is_empty(),
                            egui::Button::new(RichText::new(*action).color(palette.accent))
                                .frame(false),
                        )
                        .clicked()
                    {
                        action_clicked = Some(index);
                    }
                }
            });
        });
        ui.add_space(4.0);
        let height = ui.available_height().max(140.0);
        egui::ScrollArea::vertical()
            .id_salt(title)
            .auto_shrink(false)
            .max_height(height)
            .show(ui, |ui| {
                ui.add_sized(
                    [ui.available_width(), height],
                    egui::TextEdit::multiline(text)
                        .frame(false)
                        .hint_text(hint_text)
                        .desired_width(f32::INFINITY),
                )
                .has_focus()
            })
            .inner
    })
    .inner;
    (focused, action_clicked)
}

/// System fonts tried after egui's built-in Latin font, in order. Each entry is a list of
/// alternatives; the first one present is used. Microsoft YaHei has no Hangul, so Malgun Gothic
/// follows it to cover Korean; Segoe UI covers the IPA symbols in dictionary phonetics.
const FALLBACK_FONTS: [(&str, &[&str]); 3] = [
    ("Windows CJK", &["msyh.ttc", "Deng.ttf", "simhei.ttf"]),
    ("Windows Korean", &["malgun.ttf", "gulim.ttc"]),
    ("Windows Latin", &["segoeui.ttf", "arial.ttf"]),
];

fn install_system_fonts(ctx: &egui::Context) {
    let font_dir = std::env::var_os("WINDIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("Fonts");
    let mut fonts = egui::FontDefinitions::default();
    let mut position = 1;
    for (name, files) in FALLBACK_FONTS {
        let Some(bytes) = files
            .iter()
            .find_map(|file| fs::read(font_dir.join(file)).ok())
        else {
            continue;
        };
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(egui::FontData::from_owned(bytes)));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            if let Some(names) = fonts.families.get_mut(&family) {
                names.insert(position.min(names.len()), name.to_owned());
            }
        }
        position += 1;
    }
    ctx.set_fonts(fonts);
}

fn main() -> eframe::Result {
    if !std::env::args().any(|arg| arg == "--ui-child") {
        return background::run()
            .map_err(|error| eframe::Error::AppCreation(Box::new(std::io::Error::other(error))));
    }
    run_ui()
}

// Private UI-child test hook. Unlike APPDATA overrides, an explicit path does not depend on
// Windows Known Folder resolution and cannot accidentally load the user's real credentials.
fn ui_test_config_path(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> Result<Option<PathBuf>, String> {
    let mut args = args.into_iter();
    let mut path = None;
    while let Some(arg) = args.next() {
        if arg == "--ui-test-config" {
            if path.is_some() {
                return Err("测试配置路径不能重复指定。".into());
            }
            let value = args.next().map(PathBuf::from).ok_or("缺少测试配置路径。")?;
            if !value.is_absolute() {
                return Err("测试配置必须使用绝对路径。".into());
            }
            path = Some(value);
        }
    }
    Ok(path)
}

fn run_ui() -> eframe::Result {
    let config_path = ui_test_config_path(std::env::args_os().skip(1))
        .map_err(|error| eframe::Error::AppCreation(Box::new(std::io::Error::other(error))))?
        .unwrap_or_else(config_path);
    // winit/file dialogs expect the UI thread to be STA; its runtime is independent of the parent.
    let _apartment = winrt_runtime::Apartment::sta()
        .map_err(|error| eframe::Error::AppCreation(Box::new(std::io::Error::other(error))))?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_visible(false)
            .with_icon(
                eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon_256.png"))
                    .expect("invalid application icon"),
            )
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([760.0, 520.0]),
        ..Default::default()
    };
    eframe::run_native(
        "OCR Translate",
        options,
        Box::new(move |cc| {
            install_system_fonts(&cc.egui_ctx);
            Ok(Box::new(OcrTranslateApp::new(cc, config_path)))
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ui_test_config_override_requires_one_absolute_path() {
        let parse = |args: &[&str]| ui_test_config_path(args.iter().map(std::ffi::OsString::from));
        assert_eq!(parse(&["--ui-child"]).unwrap(), None);
        assert!(parse(&["--ui-child", "--ui-test-config"]).is_err());
        assert!(parse(&["--ui-test-config", "relative.json"]).is_err());
        let path = r"C:\isolated test\config.json";
        assert_eq!(
            parse(&["--ui-child", "--ui-test-config", path]).unwrap(),
            Some(PathBuf::from(path))
        );
        assert!(parse(&["--ui-test-config", path, "--ui-test-config", path]).is_err());
    }

    fn render_about_card(
        ctx: &egui::Context,
        width: f32,
        events: Vec<egui::Event>,
    ) -> (egui::FullOutput, egui::Rect, egui::Rect) {
        let mut card_rect = egui::Rect::NOTHING;
        let mut button_rect = egui::Rect::NOTHING;
        let output = ctx.run(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 1000.0),
                )),
                events,
                ..Default::default()
            },
            |ctx| {
                let theme = if ctx.style().visuals.dark_mode {
                    ThemeMode::Night
                } else {
                    ThemeMode::Day
                };
                egui::CentralPanel::default().show(ctx, |ui| {
                    let response = draw_about_card(ui, &Palette::of(theme));
                    card_rect = response.response.rect;
                    button_rect = response.inner.rect;
                });
            },
        );
        (output, card_rect, button_rect)
    }

    #[test]
    fn about_card_shows_version_and_website_without_horizontal_overflow() {
        for theme in [ThemeMode::Day, ThemeMode::Night] {
            for width in [375.0, 680.0] {
                for font_size in [DEFAULT_FONT_SIZE, *FONT_SIZE_RANGE.end()] {
                    let ctx = egui::Context::default();
                    install_system_fonts(&ctx);
                    apply_appearance(
                        &ctx,
                        &AppConfig {
                            theme,
                            font_size,
                            ..Default::default()
                        },
                    );
                    let _ = render_about_card(&ctx, width, Vec::new());
                    let (output, card_rect, button_rect) =
                        render_about_card(&ctx, width, Vec::new());
                    let rendered_text: Vec<_> = output
                        .shapes
                        .iter()
                        .filter_map(|shape| match &shape.shape {
                            egui::Shape::Text(text) => Some(text.galley.job.text.as_str()),
                            _ => None,
                        })
                        .collect();
                    for expected in [
                        "关于",
                        "OCR Translate",
                        &format!("v{APP_VERSION}"),
                        "官方网站 · GitHub 仓库",
                        OFFICIAL_WEBSITE,
                        "访问官方网站",
                    ] {
                        assert!(
                            rendered_text.contains(&expected),
                            "missing about text: {expected}"
                        );
                    }
                    assert!(card_rect.is_finite());
                    assert!(card_rect.left() >= 0.0 && card_rect.right() <= width + 1.0);
                    assert!(card_rect.contains_rect(button_rect));
                    assert!(
                        output.platform_output.commands.is_empty(),
                        "rendering the about card must not open the browser"
                    );
                }
            }
        }
    }

    #[test]
    fn about_website_button_opens_only_the_official_repository() {
        let ctx = egui::Context::default();
        apply_appearance(&ctx, &AppConfig::default());
        let _ = render_about_card(&ctx, 680.0, Vec::new());
        let (_, _, button_rect) = render_about_card(&ctx, 680.0, Vec::new());
        let position = button_rect.center();
        let (pressed, _, _) = render_about_card(
            &ctx,
            680.0,
            vec![
                egui::Event::PointerMoved(position),
                egui::Event::PointerButton {
                    pos: position,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
        );
        assert!(pressed.platform_output.commands.is_empty());
        let (released, _, _) = render_about_card(
            &ctx,
            680.0,
            vec![egui::Event::PointerButton {
                pos: position,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
        );
        assert_eq!(
            released.platform_output.commands,
            vec![egui::OutputCommand::OpenUrl(egui::OpenUrl::new_tab(
                OFFICIAL_WEBSITE
            ))]
        );
    }

    fn vision_page() -> layout::ImageLayout {
        layout::ImageLayout {
            width: 200,
            height: 100,
            rgba: [240, 240, 240, 255].repeat(200 * 100),
            blocks: Vec::new(),
            dispersed: false,
        }
    }

    fn model_response(content: Value) -> String {
        serde_json::json!({"choices": [{"message": {"content": content.to_string()}}]}).to_string()
    }

    #[test]
    fn closing_popup_releases_image_but_keeps_text_and_dictionary() {
        let page = Arc::new(vision_page());
        let weak_page = Arc::downgrade(&page);
        let entry = Arc::new(dictionary::Entry {
            ..Default::default()
        });
        let mut result = TranslationResult {
            source_text: "Hello world".into(),
            translated_text: "你好世界".into(),
            layout: Some(page),
            dictionary: Some(entry.clone()),
        };
        result.release_unused_image(true, false);
        assert!(
            weak_page.upgrade().is_some(),
            "open popup still needs the image"
        );
        result.release_unused_image(false, true);
        assert!(
            weak_page.upgrade().is_some(),
            "active request is not cleaned up"
        );
        result.release_unused_image(false, false);
        assert!(
            weak_page.upgrade().is_none(),
            "last image allocation was freed"
        );
        assert_eq!(result.source_text, "Hello world");
        assert_eq!(result.translated_text, "你好世界");
        assert!(Arc::ptr_eq(result.dictionary.as_ref().unwrap(), &entry));
    }

    #[test]
    fn text_view_never_allocates_a_texture_and_image_view_reuses_then_frees_it() {
        let ctx = egui::Context::default();
        let page = Arc::new(vision_page());
        let mut popup = popup_at([500, 300]);
        let size = Some(egui::vec2(100.0, 50.0));
        let baseline = ctx.tex_manager().read().num_allocated();
        for _ in 0..20 {
            popup.image_view = Some(false);
            popup.prepare_texture(&ctx, Some(&page), size);
            assert_eq!(ctx.tex_manager().read().num_allocated(), baseline);
            popup.image_view = Some(true);
            popup.prepare_texture(&ctx, Some(&page), size);
            let texture = &popup.texture.as_ref().unwrap().1;
            assert_eq!(texture.size(), [100, 50]);
            let id = texture.id();
            let bytes = ctx.tex_manager().read().meta(id).unwrap().bytes_used();
            assert_eq!(bytes, 100 * 50 * 4);
            popup.prepare_texture(&ctx, Some(&page), size);
            assert_eq!(popup.texture.as_ref().unwrap().1.id(), id);
            assert_eq!(ctx.tex_manager().read().num_allocated(), baseline + 1);
            popup.image_view = Some(false);
            popup.prepare_texture(&ctx, Some(&page), size);
            assert!(ctx.tex_manager().read().meta(id).is_none());
        }
        popup.image_view = Some(true);
        popup.prepare_texture(&ctx, Some(&page), size);
        drop(popup);
        assert_eq!(ctx.tex_manager().read().num_allocated(), baseline);
    }

    #[test]
    fn replacing_capture_or_display_size_does_not_accumulate_textures() {
        let ctx = egui::Context::default();
        ctx.set_pixels_per_point(2.0);
        // Apply the requested DPI before preparing textures.
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        let baseline = ctx.tex_manager().read().num_allocated();
        let mut popup = popup_at([500, 300]);
        popup.image_view = Some(true);
        for _ in 0..30 {
            let page = Arc::new(vision_page());
            let previous_id = popup.texture.as_ref().map(|(_, texture)| texture.id());
            popup.prepare_texture(&ctx, Some(&page), Some(egui::vec2(50.0, 25.0)));
            let old_id = popup.texture.as_ref().unwrap().1.id();
            assert_ne!(previous_id, Some(old_id));
            assert_eq!(popup.texture.as_ref().unwrap().1.size(), [100, 50]);
            popup.prepare_texture(&ctx, Some(&page), Some(egui::vec2(100.0, 50.0)));
            assert_eq!(popup.texture.as_ref().unwrap().1.size(), [200, 100]);
            assert!(ctx.tex_manager().read().meta(old_id).is_none());
            assert_eq!(ctx.tex_manager().read().num_allocated(), baseline + 1);
        }
        popup.prepare_texture(&ctx, None, None);
        assert_eq!(ctx.tex_manager().read().num_allocated(), baseline);
    }

    #[test]
    fn vision_image_response_maps_blocks_and_derives_missing_full_text() {
        let response = model_response(serde_json::json!({"blocks": [
            {"bbox": [100, 200, 400, 300], "source_text": "Hello world", "translated_text": "你好世界"},
            {"bbox": "bad", "source_text": "Broken", "translated_text": "忽略"}
        ]}));
        let result = parse_image_translation_response(&response, vision_page()).unwrap();
        assert_eq!(result.source_text, "Hello world");
        assert_eq!(result.translated_text, "你好世界");
        let page = result.layout.unwrap();
        assert_eq!(page.blocks.len(), 1);
        assert_eq!(page.blocks[0].rect, [20.0, 20.0, 80.0, 30.0]);
    }

    #[test]
    fn legacy_vision_response_keeps_the_image_and_full_translation() {
        let response = model_response(serde_json::json!({
            "original": "Hello world", "translation": "你好世界",
            "blocks": [{"bbox": [0, 0, 0, 0], "translated_text": "无效位置"}]
        }));
        let result = parse_image_translation_response(&response, vision_page()).unwrap();
        assert_eq!(result.source_text, "Hello world");
        assert_eq!(result.translated_text, "你好世界");
        let page = result.layout.unwrap();
        assert_eq!((page.width, page.height), (200, 100));
        assert!(page.blocks.is_empty());
    }

    #[test]
    fn vision_image_request_uses_one_multimodal_call_without_an_ocr_pack() {
        use std::io::{BufRead, BufReader, Read, Write};
        use std::net::TcpListener;

        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        server.set_nonblocking(true).unwrap();
        let url = format!(
            "http://{}/v1/chat/completions",
            server.local_addr().unwrap()
        );
        let response = model_response(serde_json::json!({
            "source_text": "Hello world", "translated_text": "你好世界",
            "blocks": [{"bbox": [100, 200, 400, 300], "source_text": "Hello world", "translated_text": "你好世界"}]
        }));
        let worker = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(10);
            let (mut stream, _) = loop {
                match server.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "mock endpoint was not called");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock endpoint failed: {error}"),
                }
            };
            // Accepted sockets can inherit nonblocking mode on Windows.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut length = 0;
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse::<usize>().unwrap();
                    }
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let request: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(request["model"], "test-vision");
            let content = &request["messages"][0]["content"];
            assert_eq!(content[0]["type"], "text");
            assert!(content[0]["text"].as_str().unwrap().contains("bbox"));
            assert_eq!(content[1]["type"], "image_url");
            assert!(content[1]["image_url"]["url"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,"));
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", response.len(), response).unwrap();
        });
        let path = std::env::temp_dir().join(format!("ocr-vision-test-{}.png", std::process::id()));
        image::RgbaImage::from_pixel(200, 100, image::Rgba([240, 240, 240, 255]))
            .save(&path)
            .unwrap();
        let config = AppConfig {
            mode: TranslationMode::Vision,
            ocr_language: "invalid-ocr-language".into(),
            api_url: url,
            api_key: "test-key".into(),
            model: "test-vision".into(),
            ..Default::default()
        };
        let result = translate_image(&config, &path);
        let _ = fs::remove_file(path);
        worker.join().unwrap();
        let result = result.unwrap();
        assert_eq!(result.translated_text, "你好世界");
        assert_eq!(
            result.layout.unwrap().blocks[0].rect,
            [20.0, 20.0, 80.0, 30.0]
        );
    }

    #[test]
    fn translated_image_paints_overlays_and_original_comparison_hides_them() {
        let response = model_response(serde_json::json!({"blocks": [
            {"bbox": [100, 200, 400, 300], "source_text": "Hello world", "translated_text": "Translated"}
        ]}));
        let page = parse_image_translation_response(&response, vision_page())
            .unwrap()
            .layout
            .unwrap();
        let ctx = egui::Context::default();
        let texture = ctx.load_texture(
            "vision-test",
            egui::ColorImage::from_rgba_unmultiplied([200, 100], &page.rgba),
            Default::default(),
        );
        let render = |original| {
            ctx.run(egui::RawInput::default(), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    paint_translated_capture(
                        ui,
                        &texture,
                        &page,
                        egui::vec2(200.0, 100.0),
                        original,
                    );
                });
            })
        };
        let translated = render(false);
        assert!(translated.shapes.iter().any(|shape| matches!(&shape.shape, egui::Shape::Text(text) if text.galley.job.text == "Translated")));
        let original = render(true);
        assert!(!original
            .shapes
            .iter()
            .any(|shape| matches!(&shape.shape, egui::Shape::Text(_))));
    }

    #[test]
    fn parses_structured_translation() {
        let response = r#"{"choices":[{"message":{"content":"{\"source_text\":\"Hello\",\"translated_text\":\"你好\"}"}}]}"#;
        let result = parse_translation_response(response).unwrap();
        assert_eq!(result.source_text, "Hello");
        assert_eq!(result.translated_text, "你好");
    }

    #[test]
    fn parses_fenced_translation_with_common_aliases() {
        let response = r#"{"choices":[{"message":{"content":"```json\n{\"original\":\"Good morning\",\"translation\":\"早上好\"}\n```"}}]}"#;
        let result = parse_translation_response(response).unwrap();
        assert_eq!(result.source_text, "Good morning");
        assert_eq!(result.translated_text, "早上好");
    }

    #[test]
    fn rejects_unstructured_model_text() {
        let response = r#"{"choices":[{"message":{"content":"Just a translation"}}]}"#;
        assert!(parse_translation_response(response).is_err());
    }

    #[test]
    fn old_config_without_volcengine_keys_still_loads() {
        let config: AppConfig = serde_json::from_str(
            r#"{"mode":"tencent","ocr_language":"en-US","target_language":"简体中文"}"#,
        )
        .unwrap();
        assert!(config.volcengine_access_key.is_empty());
        assert!(config.volcengine_secret_key.is_empty());
        assert_eq!(config.theme, ThemeMode::Night);
        assert_eq!(config.font_size, DEFAULT_FONT_SIZE);
        assert_eq!(config.close_behavior, CloseBehavior::Tray);
        assert!(config.two_way);
        assert_eq!(config.second_language, "英语");
    }

    #[test]
    fn two_way_translation_turns_text_in_the_target_language_around() {
        let config = AppConfig::default();
        assert_eq!(target_for(&config, "Hello world"), Some("简体中文"));
        assert_eq!(target_for(&config, "こんにちは、世界"), Some("简体中文"));
        assert_eq!(target_for(&config, "你好，世界"), Some("英语"));
        assert_eq!(target_for(&config, "12:30"), None);
        let japanese = AppConfig {
            target_language: "日语".into(),
            second_language: "简体中文".into(),
            ..Default::default()
        };
        assert_eq!(target_for(&japanese, "你好，世界"), Some("日语"));
        assert_eq!(target_for(&japanese, "こんにちは、世界"), Some("简体中文"));
        let one_way = AppConfig {
            two_way: false,
            ..Default::default()
        };
        assert_eq!(target_for(&one_way, "你好，世界"), Some("简体中文"));
    }

    #[test]
    fn directed_settings_settle_the_target_or_leave_it_to_a_vision_model() {
        let config = AppConfig::default();
        let settled = directed(&config, "你好");
        assert_eq!(settled.target_language, "英语");
        assert!(!settled.two_way);
        assert_eq!(vision_target(&settled), "英语");
        // Without recognized text the vision model gets both languages and the rule.
        let open = directed(&config, "");
        assert!(open.two_way);
        assert_eq!(
            vision_target(&open),
            "简体中文（如果原文本身就是简体中文，则翻译成英语）"
        );
        // A free-form target of unknown script leaves the direction to the model as well.
        let custom = AppConfig {
            target_language: "意大利语".into(),
            ..Default::default()
        };
        assert!(directed(&custom, "Ciao a tutti").two_way);
    }

    #[test]
    #[ignore = "uses the live Tencent and Google translation endpoints"]
    fn translates_live_text_both_ways() {
        for mode in [TranslationMode::Tencent, TranslationMode::Google] {
            let config = AppConfig {
                mode,
                ..Default::default()
            };
            let english = translate_text(&config, "你好，世界").unwrap();
            assert!(english.to_lowercase().contains("world"), "{english}");
            let chinese = translate_text(&config, "Good morning, world").unwrap();
            assert!(chinese.contains('世'), "{chinese}");
            let from_japanese = translate_text(&config, "おはようございます、世界").unwrap();
            assert!(from_japanese.contains('世'), "{from_japanese}");
        }
    }

    #[test]
    fn close_behavior_survives_protected_config_round_trip() {
        for behavior in [CloseBehavior::Tray, CloseBehavior::Exit] {
            let config = AppConfig {
                close_behavior: behavior,
                ..Default::default()
            };
            let json = serde_json::to_string(&config.protected().unwrap()).unwrap();
            let restored: AppConfig = serde_json::from_str(&json).unwrap();
            assert_eq!(restored.close_behavior, behavior);
        }
    }

    #[test]
    fn removed_ask_behavior_migrates_without_losing_other_settings() {
        let config: AppConfig = serde_json::from_str(
            r#"{"close_behavior":"ask","font_size":18,"api_key":"existing-key"}"#,
        )
        .unwrap();
        assert_eq!(config.close_behavior, CloseBehavior::Tray);
        assert_eq!(config.font_size, 18);
        assert_eq!(config.api_key, "existing-key");
    }

    #[test]
    fn appearance_settings_round_trip() {
        let config = AppConfig {
            theme: ThemeMode::Day,
            font_size: 18,
            ..Default::default()
        };
        let restored: AppConfig =
            serde_json::from_str(&serde_json::to_string(&config).unwrap()).unwrap();
        assert_eq!(restored.theme, ThemeMode::Day);
        assert_eq!(restored.font_size, 18);
    }

    #[test]
    fn saved_config_encrypts_secrets_and_reveals_them_on_load() {
        let config = AppConfig {
            api_key: "vision-key".into(),
            volcengine_access_key: "access-key".into(),
            volcengine_secret_key: "secret-key".into(),
            ..Default::default()
        };
        let json = serde_json::to_string(&config.protected().unwrap()).unwrap();
        assert!(!json.contains("vision-key") && !json.contains("secret-key"));

        let mut loaded: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.reveal_secrets(), (false, false));
        assert_eq!(loaded.api_key, "vision-key");
        assert_eq!(loaded.volcengine_access_key, "access-key");
        assert_eq!(loaded.volcengine_secret_key, "secret-key");
    }

    #[test]
    fn plaintext_secrets_from_old_versions_are_flagged_for_migration() {
        let mut config: AppConfig =
            serde_json::from_str(r#"{"volcengine_secret_key":"old-plaintext"}"#).unwrap();
        assert_eq!(config.reveal_secrets(), (true, false));
        assert_eq!(config.volcengine_secret_key, "old-plaintext");
    }

    #[test]
    fn undecryptable_secrets_are_cleared() {
        let mut config: AppConfig = serde_json::from_str(r#"{"api_key":"dpapi:AAAA"}"#).unwrap();
        assert_eq!(config.reveal_secrets(), (false, true));
        assert!(config.api_key.is_empty());
    }

    #[test]
    fn formats_character_counts_in_wan() {
        assert_eq!(format_chars(16_207.0), "1.6 万");
        assert_eq!(format_chars(2_000_000.0), "200 万");
        assert_eq!(format_chars(1_983_793.0), "198.4 万");
        assert_eq!(format_chars(820.0), "820");
    }

    fn popup_at(anchor: [i32; 2]) -> ResultPopup {
        ResultPopup {
            id: 1,
            task_id: None,
            anchor,
            work_area: [0, 0, 1920, 1040],
            opened_at: Instant::now(),
            pinned: false,
            show_source: false,
            copied: false,
            placed: None,
            shown: false,
            width: POPUP_WIDTH,
            height: 120.0,
            drag_origin: None,
            resize_origin: None,
            moved: false,
            user_sized: false,
            image_view: None,
            texture: None,
            result: TranslationResult::default(),
            pending: false,
            provisional: false,
            status: String::new(),
            escape_was_down: false,
            click_was_down: false,
            close_requested: false,
        }
    }

    fn placed_popup(anchor: [i32; 2]) -> ResultPopup {
        let mut popup = popup_at(anchor);
        popup.placed = Some(egui::Rect::from_min_size(
            egui::pos2(500.0, 300.0),
            egui::vec2(380.0, 120.0),
        ));
        popup
    }

    #[test]
    fn popup_opens_below_right_of_the_selection_end() {
        let corner = popup_at([500, 300]).place(egui::vec2(400.0, 200.0));
        assert_eq!(corner, egui::pos2(516.0, 316.0));
    }

    #[test]
    fn a_shown_popup_keeps_its_corner_and_only_moves_to_stay_on_screen() {
        let popup = popup_at([500, 900]);
        let corner = egui::pos2(516.0, 700.0);
        assert_eq!(popup.settle(corner, egui::vec2(400.0, 200.0)), corner);
        // Growing past the bottom moves it up by the overflow, not above the anchor.
        assert_eq!(
            popup.settle(corner, egui::vec2(400.0, 500.0)),
            egui::pos2(516.0, 540.0)
        );
        assert_eq!(
            popup.settle(egui::pos2(1700.0, 100.0), egui::vec2(400.0, 200.0)),
            egui::pos2(1520.0, 100.0)
        );
    }

    #[test]
    fn popup_flips_to_stay_inside_the_work_area() {
        let corner = popup_at([1900, 1000]).place(egui::vec2(400.0, 200.0));
        assert_eq!(corner, egui::pos2(1484.0, 784.0));
    }

    #[test]
    fn new_result_popup_avoids_a_pinned_popup_at_the_same_capture_anchor() {
        let mut pinned = placed_popup([500, 300]);
        pinned.pinned = true;
        pinned.placed = Some(egui::Rect::from_min_size(
            egui::pos2(516.0, 316.0),
            egui::vec2(400.0, 200.0),
        ));
        let existing = pinned.placed.unwrap();
        let next = popup_at([500, 300]);
        let corner = next.place_avoiding(egui::vec2(400.0, 200.0), &[existing]);
        let result = egui::Rect::from_min_size(corner, egui::vec2(400.0, 200.0));
        assert!(!result.intersects(existing.expand(6.0)));
        assert!(corner.x >= 0.0 && corner.y >= 0.0);
        assert!(result.right() <= 1920.0 && result.bottom() <= 1040.0);
    }

    #[test]
    fn capture_start_keeps_pinned_popups_and_cancel_removes_only_provisionals() {
        let mut pinned = placed_popup([500, 300]);
        pinned.id = 1;
        pinned.pinned = true;
        let mut provisional = placed_popup([500, 300]);
        provisional.id = 2;
        provisional.provisional = true;
        let mut popups = vec![pinned, provisional];

        retain_pinned_popups(&mut popups);
        assert_eq!(popups.len(), 1);
        assert_eq!(popups[0].id, 1);
        assert!(popups[0].pinned);

        let mut pending = placed_popup([500, 300]);
        pending.id = 3;
        pending.provisional = true;
        popups.push(pending);
        discard_provisional_popups(&mut popups);
        assert_eq!(popups.len(), 1);
        assert_eq!(popups[0].id, 1);
    }

    #[test]
    fn restored_popup_adopts_the_completed_session_result() {
        let mut popup = popup_at([500, 300]);
        popup.pending = true;
        popup.provisional = true;
        restore_popup_result(
            &mut popup,
            TranslationResult {
                source_text: "source".into(),
                translated_text: "translation".into(),
                ..Default::default()
            },
            "翻译完成。",
        );
        assert!(!popup.pending && !popup.provisional);
        assert_eq!(popup.result.source_text, "source");
        assert_eq!(popup.result.translated_text, "translation");
    }

    #[test]
    fn dragging_uses_absolute_screen_cursor_and_keeps_its_monitor_bounds() {
        let ctx = egui::Context::default();
        let mut popup = placed_popup([500, 300]);
        popup.begin_drag([100, 100]);
        popup.apply_drag(&ctx, [140, 120]);
        assert_eq!(popup.placed.unwrap().min, egui::pos2(540.0, 320.0));

        // Repeated coordinates after a window move cannot accumulate or bounce it back.
        popup.apply_drag(&ctx, [140, 120]);
        assert_eq!(popup.placed.unwrap().min, egui::pos2(540.0, 320.0));
        popup.apply_drag(&ctx, [150, 125]);
        assert_eq!(popup.placed.unwrap().min, egui::pos2(550.0, 325.0));
        assert!(popup.moved);

        popup.apply_drag(&ctx, [10000, -10000]);
        assert_eq!(popup.placed.unwrap().min, egui::pos2(1540.0, 0.0));
    }

    #[test]
    fn resizing_uses_absolute_cursor_and_updates_both_builder_dimensions() {
        let ctx = egui::Context::default();
        let mut popup = placed_popup([500, 300]);
        popup.begin_resize([100, 100]);
        popup.apply_resize(&ctx, [-5000, -5000]);
        assert_eq!(popup.placed.unwrap().size(), egui::vec2(240.0, 140.0));
        assert_eq!((popup.width, popup.height), (240.0, 140.0));

        popup.apply_resize(&ctx, [5100, 5100]);
        assert_eq!(popup.placed.unwrap().size(), egui::vec2(1420.0, 740.0));
        assert_eq!((popup.width, popup.height), (1420.0, 740.0));
    }
}
