//! About window: `veracage-agent _about` (Help → About Veracage).
//! Shows the app icon, name, version, and a GitHub link.

use std::sync::Arc;

use eframe::egui;

use crate::config;
use crate::gl_icon::GlIcon;

/// The icon's displayed square side, in logical points.
const ICON_PT: f32 = 96.0;

const GITHUB_URL: &str = "https://github.com/emptyname-org/veracage";
// The shared 192px app icon (same file the compositor uses), drawn with a
// direct GL call (gl_icon) so it has no egui_glow sRGB-texture fringe.
const ICON_PNG: &[u8] = include_bytes!("../../Icons/veracage_icon.png");

pub fn run() -> Result<(), eframe::Error> {
    let icon = eframe::icon_data::from_png_bytes(ICON_PNG).ok();
    let mut vp = egui::ViewportBuilder::default()
        .with_title("About Veracage")
        .with_app_id("veracage")
        .with_inner_size([440.0, 380.0])
        .with_resizable(false);
    if let Some(i) = icon.clone() {
        vp = vp.with_icon(i);
    }
    let options = eframe::NativeOptions { viewport: vp, ..Default::default() };
    let cfg = config::load();
    eframe::run_native(
        "veracage-about",
        options,
        Box::new(move |cc| {
            crate::theme::apply_config(&cc.egui_ctx, &cfg);
            // Draw the logo with a direct GL call (no egui_glow sRGB fringe).
            // eframe always hands us a glow context on a real desktop, so this
            // is None only in the pathological no-GL case, where the logo is
            // simply absent (no second, fringe-y egui-Image path to maintain).
            let gl_icon = match (&cc.gl, &icon) {
                (Some(gl), Some(i)) => {
                    GlIcon::new(gl, i.width as i32, i.height as i32, &i.rgba).map(Arc::new)
                }
                _ => None,
            };
            Ok(Box::new(About { gl_icon }) as Box<dyn eframe::App>)
        }),
    )
}

struct About {
    gl_icon: Option<Arc<GlIcon>>,
}

impl eframe::App for About {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        crate::theme::content_panel(ctx, |ui| {
                ui.add_space(16.0);
                ui.vertical_centered(|ui| {
                    let (rect, _) =
                        ui.allocate_exact_size(egui::vec2(ICON_PT, ICON_PT), egui::Sense::hover());
                    if let Some(icon) = &self.gl_icon {
                        let icon = icon.clone();
                        ui.painter().add(egui::PaintCallback {
                            rect,
                            callback: Arc::new(eframe::egui_glow::CallbackFn::new(
                                move |info, painter| {
                                    let vp = info.viewport_in_pixels();
                                    icon.paint(
                                        painter.gl(),
                                        vp.left_px,
                                        vp.from_bottom_px,
                                        vp.width_px,
                                        vp.height_px,
                                    );
                                },
                            )),
                        });
                    }
                    ui.add_space(6.0);
                    ui.heading("Veracage");
                    ui.label("Encrypted volumes, isolated.");
                    ui.add_space(6.0);
                    // The one thing a user can lose work to, so it is said here too
                    // (Help has the full section).
                    ui.label("Veracage stores nothing itself: files are kept only in\na mounted volume or the shared directory.");
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!("version {}", env!("CARGO_PKG_VERSION"))).weak(),
                    );
                    ui.add_space(14.0);
                    ui.hyperlink_to("GitHub", GITHUB_URL);
                });
            });
    }
}
