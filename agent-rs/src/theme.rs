//! egui house style for the agent's transient windows: theme colors, rounded
//! widgets, roomy spacing, and a text scale derived from the configured base
//! point size (which defaults to the host desktop's - see `fonts`). No absolute
//! size is hardcoded; only the typographic ratios (heading vs body) are.

use eframe::egui;

/// Veracage accent (the turquoise of the app icon).
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x21, 0x9e, 0x96);
/// Error text (form validation, wrong passphrase).
pub const ERROR: egui::Color32 = egui::Color32::from_rgb(0xc0, 0x45, 0x45);

/// Full style pass for a one-shot window: font family + theme + sizes. Resolves
/// the base size once (may shell out to read the host size), so call per window,
/// not per frame - live dialogs (Settings) cache the base and call the pieces.
pub fn apply_config(ctx: &egui::Context, cfg: &crate::config::Config) {
    crate::fonts::install(ctx, &cfg.ui_font);
    apply_theme(ctx, &cfg.theme);
    set_text_sizes(ctx, crate::fonts::base_size(&cfg.ui_font, &cfg.ui_font_size));
}

/// Theme colors, widget rounding, and spacing (no fonts). Cheap to call per frame.
pub fn apply_theme(ctx: &egui::Context, theme: &str) {
    // A slow click is still a click. egui drops a press-release pair that lasted
    // longer than `max_click_duration` (0.8s by default) so that a touch screen
    // can use press-and-hold for a context menu - which cost real Save and
    // Cancel clicks here: the debug trace showed a press and release both on the
    // button, 2 seconds apart, with `clicked=false`. These dialogs have no
    // context menus, so the timeout only has downside.
    ctx.options_mut(|o| o.input_options.max_click_duration = f64::INFINITY);
    let dark = theme == "dark";
    let mut v = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };

    if dark {
        v.panel_fill = egui::Color32::from_rgb(0x20, 0x23, 0x25);
        v.window_fill = v.panel_fill;
        v.extreme_bg_color = egui::Color32::from_rgb(0x17, 0x1a, 0x1c);
        v.faint_bg_color = egui::Color32::from_rgb(0x2a, 0x2e, 0x31);
    } else {
        v.panel_fill = egui::Color32::from_rgb(0xfa, 0xfa, 0xf8);
        v.window_fill = v.panel_fill;
        v.extreme_bg_color = egui::Color32::WHITE;
        v.faint_bg_color = egui::Color32::from_rgb(0xf0, 0xf1, 0xef);
    }

    v.selection.bg_fill = ACCENT.gamma_multiply(if dark { 0.55 } else { 0.35 });
    v.selection.stroke = egui::Stroke::new(1.0, ACCENT);
    v.hyperlink_color = ACCENT;

    let r = egui::Rounding::same(6.0);
    v.widgets.noninteractive.rounding = r;
    v.widgets.inactive.rounding = r;
    v.widgets.hovered.rounding = r;
    v.widgets.active.rounding = r;
    v.widgets.open.rounding = r;
    v.window_rounding = egui::Rounding::same(10.0);

    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.0, ACCENT.gamma_multiply(0.7));
    v.widgets.active.bg_stroke = egui::Stroke::new(1.0, ACCENT);

    ctx.set_visuals(v);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(10.0, 10.0);
        s.spacing.button_padding = egui::vec2(14.0, 6.0);
        s.spacing.interact_size = egui::vec2(44.0, 26.0);
        s.spacing.icon_width = 22.0;
        s.spacing.icon_width_inner = 14.0;
        s.spacing.combo_width = 160.0;
    });
}

/// Set every text style from a base point size (the body size). Headings and the
/// small style scale from it, so nothing is an absolute hardcoded size. Cheap.
pub fn set_text_sizes(ctx: &egui::Context, base: f32) {
    use egui::{FontFamily::Monospace, FontFamily::Proportional, FontId, TextStyle};
    let base = base.clamp(6.0, 48.0);
    ctx.style_mut(|s| {
        s.text_styles.insert(TextStyle::Small, FontId::new((base * 0.85).round(), Proportional));
        s.text_styles.insert(TextStyle::Body, FontId::new(base, Proportional));
        s.text_styles.insert(TextStyle::Button, FontId::new(base, Proportional));
        s.text_styles.insert(TextStyle::Heading, FontId::new((base * 1.5).round(), Proportional));
        s.text_styles.insert(TextStyle::Monospace, FontId::new(base, Monospace));
    });
}

/// Open a one-shot dialog window in the house style: the shared `veracage` app
/// id (so the desktop groups every window as one app), a title, a starting size
/// and a floor. Every dialog is its own process because winit cannot reopen an
/// EventLoop, so this is the one place that setup lives.
pub fn dialog(
    id: &str,
    title: &str,
    size: [f32; 2],
    min_size: [f32; 2],
    app: impl FnOnce(&eframe::CreationContext<'_>) -> Box<dyn eframe::App> + 'static,
) -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_app_id("veracage")
            .with_inner_size(size)
            .with_min_inner_size(min_size),
        ..Default::default()
    };
    eframe::run_native(id, options, Box::new(|cc| Ok(app(cc))))
}

/// The font/theme state a dialog needs when it can CHANGE them (Settings,
/// Appearance): installing a font rebuilds the atlas and resolving the base size
/// may shell out to read the host's, so both happen only when the selection
/// actually changes. Dialogs that cannot change them call `apply_config` once.
pub struct LiveStyle {
    font: Option<String>,
    size: Option<String>,
    base: f32,
}

impl Default for LiveStyle {
    fn default() -> Self {
        Self {
            font: None,
            size: None,
            base: crate::fonts::FALLBACK_SIZE,
        }
    }
}

impl LiveStyle {
    /// Bring the context in line with `cfg`. Call once per frame.
    pub fn apply(&mut self, ctx: &egui::Context, cfg: &crate::config::Config) {
        let font_changed = self.font.as_deref() != Some(cfg.ui_font.as_str());
        let size_changed = self.size.as_deref() != Some(cfg.ui_font_size.as_str());
        if font_changed {
            crate::fonts::install(ctx, &cfg.ui_font);
            self.font = Some(cfg.ui_font.clone());
        }
        if font_changed || size_changed {
            self.base = crate::fonts::base_size(&cfg.ui_font, &cfg.ui_font_size);
            self.size = Some(cfg.ui_font_size.clone());
        }
        apply_theme(ctx, &cfg.theme);
        set_text_sizes(ctx, self.base);
    }
}

/// The widest of `texts` in the body font. Used to size a dialog's label and
/// control columns from their actual content rather than a hardcoded width.
pub fn text_width(ui: &egui::Ui, texts: &[&str]) -> f32 {
    let font = egui::TextStyle::Body.resolve(ui.style());
    texts
        .iter()
        .map(|text| {
            ui.fonts(|f| {
                f.layout_no_wrap((*text).to_string(), font.clone(), egui::Color32::PLACEHOLDER)
                    .size()
                    .x
            })
        })
        .fold(0.0_f32, f32::max)
}

/// The height every widget in a settings row is given: enough for a line of
/// text plus a button's padding, and never less than egui's own minimum for
/// something clickable.
pub fn row_height(ui: &egui::Ui) -> f32 {
    let text = ui.text_style_height(&egui::TextStyle::Body);
    (text + 2.0 * ui.spacing().button_padding.y).max(ui.spacing().interact_size.y)
}

/// Vertical gap between two settings rows.
pub const ROW_GAP: f32 = 10.0;

/// One settings row: the label, then its control in an aligned column.
///
/// Neither of egui's own answers aligns these: a `Grid` top-aligns its cells
/// (emilk/egui#2247) and a horizontal row does not reliably centre widgets of
/// different heights, "especially when the font size is large"
/// (emilk/egui#7412) - which is exactly these dialogs at 14pt. So instead of
/// aligning differently-sized widgets, give them ONE height: the label goes in a
/// box of that height, laid out left-to-right with a centred cross axis, and
/// `interact_size.y` makes every control in the row match.
pub fn row(ui: &mut egui::Ui, width: f32, label: &str, control: impl FnOnce(&mut egui::Ui)) {
    labelled_row(
        ui,
        width,
        |ui| {
            ui.label(label);
        },
        control,
    );
}

/// `row` with a widget instead of a plain label on the left, for a row whose
/// "label" is a checkbox.
pub fn labelled_row(
    ui: &mut egui::Ui,
    width: f32,
    label: impl FnOnce(&mut egui::Ui),
    control: impl FnOnce(&mut egui::Ui),
) {
    ui.horizontal(|ui| {
        let h = row_height(ui);
        ui.spacing_mut().interact_size.y = h;
        // The box shrinks to the text, so pad out to the column width after it.
        let left = ui.cursor().left();
        ui.allocate_ui_with_layout(
            egui::vec2(width, h),
            egui::Layout::left_to_right(egui::Align::Center),
            label,
        );
        let pad = width - (ui.cursor().left() - left);
        if pad > 0.0 {
            ui.add_space(pad);
        }
        control(ui);
    });
    ui.add_space(ROW_GAP);
}

/// The house margin between a dialog's content and the window edge, used on the
/// left, the right AND the top, so every window sits in an even frame.
const SIDE_MARGIN: f32 = 36.0;

/// The margin between two stacked panels (the content panel's bottom and the
/// action bar's own edges): half the side margin, because the two add up.
const PANEL_GAP: f32 = SIDE_MARGIN / 2.0;

/// How far the scroll bar sits from the right window edge. The content keeps the
/// full side margin: this much on the panel, the rest inside the scroll area.
const SCROLLBAR_INSET: f32 = 20.0;

/// The house style for a dialog's action bar: the side margins, and the panel
/// gap above and below the buttons.
pub fn content_frame(ctx: &egui::Context) -> egui::Frame {
    egui::Frame::central_panel(&ctx.style())
        .inner_margin(egui::Margin::symmetric(SIDE_MARGIN, PANEL_GAP))
}

/// A dialog's scrollable content panel: the house frame plus a vertical scroll
/// area, so a window shorter than its content never hides the bottom rows (the
/// Save/Cancel buttons live in `action_bar`, a separate pinned panel). Route
/// every dialog body through this, so no dialog - present or future - can overflow
/// off-screen, and so the margins below are the ONE place they are set.
pub fn content_panel(ctx: &egui::Context, add_contents: impl FnOnce(&mut egui::Ui)) {
    egui::CentralPanel::default()
        .frame(
            egui::Frame::central_panel(&ctx.style())
                .inner_margin(egui::Margin {
                    left: SIDE_MARGIN,
                    right: SCROLLBAR_INSET,
                    top: SIDE_MARGIN,
                    bottom: PANEL_GAP,
                }),
        )
        .show(ctx, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    egui::Frame::none()
                        .inner_margin(egui::Margin {
                            left: 0.0,
                            right: SIDE_MARGIN - SCROLLBAR_INSET,
                            top: 0.0,
                            bottom: 0.0,
                        })
                        .show(ui, add_contents);
                });
        });
}

/// Roomier internal padding for the Save/Cancel buttons.
pub fn pad_buttons(ui: &mut egui::Ui) {
    ui.spacing_mut().button_padding = egui::vec2(16.0, 6.0);
}

/// The shared dialog action bar (bottom panel): Save / Cancel right-aligned,
/// with a caller-drawn `trailing` widget to their left (an error status, an
/// "N enabled" count, ...). Returns `(save_clicked, cancel_clicked)`.
///
/// Enter also saves and Escape also cancels, but only while no widget holds
/// keyboard focus, so Enter still belongs to a focused text field (the
/// Configure-apps "Add" box). Besides being the expected dialog behaviour, the
/// keyboard route always lands: a click can be spent on something else first
/// (activating the window, or dismissing an open dropdown), which is what makes
/// Save look like it needs pressing twice.
pub fn action_bar(ctx: &egui::Context, trailing: impl FnOnce(&mut egui::Ui)) -> (bool, bool) {
    let typing = ctx.memory(|m| m.focused().is_some());
    let (mut save, mut cancel) = if typing {
        (false, false)
    } else {
        ctx.input(|i| {
            (
                i.key_pressed(egui::Key::Enter),
                i.key_pressed(egui::Key::Escape),
            )
        })
    };
    let mut buttons = None;
    egui::TopBottomPanel::bottom("actions")
        .frame(content_frame(ctx))
        .show(ctx, |ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                pad_buttons(ui);
                let cancel_resp = ui.button("Cancel");
                let save_resp = ui.button("Save");
                cancel |= cancel_resp.clicked();
                save |= save_resp.clicked();
                buttons = Some((save_resp, cancel_resp));
                trailing(ui);
            });
        });
    if let Some((save_resp, cancel_resp)) = buttons {
        log_action_bar(ctx, &save_resp, &cancel_resp, typing);
    }
    (save, cancel)
}

/// With config `debug` on, log what the pointer did to the action bar.
///
/// This exists for the "Save/Cancel needs a second click" report, whose three
/// candidates look different here: a window that had no keyboard focus when the
/// press arrived (the desktop consumed the click to activate it) shows
/// `focused=false`; an open dropdown eating the click shows `popup_open=true`;
/// and a press egui does not turn into a click shows `over_save=true` with
/// `save_clicked=false`. Silent unless a button went down or up this frame.
fn log_action_bar(
    ctx: &egui::Context,
    save: &egui::Response,
    cancel: &egui::Response,
    typing: bool,
) {
    let (pressed, released, pos) = ctx.input(|i| {
        (
            i.pointer.any_pressed(),
            i.pointer.any_released(),
            i.pointer.interact_pos(),
        )
    });
    if !pressed && !released {
        return;
    }
    let focused = ctx.input(|i| i.focused);
    let popup_open = ctx.memory(|m| m.any_popup_open());
    // How long the button was held: the reason a click can go missing.
    let now = ctx.input(|i| i.time);
    let press_id = egui::Id::new("veracage_action_press");
    if pressed {
        ctx.data_mut(|d| d.insert_temp(press_id, now));
    }
    let held = ctx
        .data(|d| d.get_temp::<f64>(press_id))
        .map(|start| now - start)
        .unwrap_or(0.0);
    let over = |r: &egui::Response| pos.is_some_and(|p| r.rect.contains(p));
    crate::broker::debug_log(&format!(
        "action bar: {}{} at {:?} held={held:.2}s focused={focused} popup_open={popup_open} \
         typing={typing} over_save={} over_cancel={} save_clicked={} cancel_clicked={}",
        if pressed { "press" } else { "" },
        if released { " release" } else { "" },
        pos.map(|p| (p.x as i32, p.y as i32)),
        over(save),
        over(cancel),
        save.clicked(),
        cancel.clicked(),
    ));
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_slow_click_still_counts() {
        // egui drops a press-release pair held longer than max_click_duration
        // (0.8s) to leave room for touch press-and-hold. That silently ate Save
        // and Cancel clicks, so every dialog turns it off through apply_theme.
        let ctx = eframe::egui::Context::default();
        super::apply_theme(&ctx, "dark");
        assert!(
            ctx.options(|o| o.input_options.max_click_duration).is_infinite(),
            "a held click must still register"
        );
    }
}
