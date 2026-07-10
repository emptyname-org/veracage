//! About window: `veracage-agent _about` (Help → About Veracage).
//! Shows the app icon, name, version, and a GitHub link.

use eframe::egui;

use crate::config;

// TODO: set to the real repo URL before the first GitHub push.
const GITHUB_URL: &str = "https://github.com/veracage/veracage";
const ICON_PNG: &[u8] =
    include_bytes!("../../Icons/veracage_icon_turquoise_transparent_corners.png");

pub fn run() -> Result<(), eframe::Error> {
    let icon = eframe::icon_data::from_png_bytes(ICON_PNG).ok();
    // Reuse the decoded RGBA for the in-window logo (no image-loader dependency).
    let logo = icon.as_ref().map(|i| {
        egui::ColorImage::from_rgba_unmultiplied(
            [i.width as usize, i.height as usize],
            &i.rgba,
        )
    });
    let mut vp = egui::ViewportBuilder::default()
        .with_title("About Veracage")
        .with_app_id("veracage")
        .with_inner_size([440.0, 380.0])
        .with_resizable(false);
    if let Some(i) = icon {
        vp = vp.with_icon(i);
    }
    let options = eframe::NativeOptions { viewport: vp, ..Default::default() };
    let theme = config::load().theme;
    eframe::run_native(
        "veracage-about",
        options,
        Box::new(move |cc| {
            crate::theme::apply(&cc.egui_ctx, &theme);
            let tex = logo.map(|img| {
                cc.egui_ctx.load_texture("veracage-logo", img, egui::TextureOptions::LINEAR)
            });
            Ok(Box::new(About { tex }) as Box<dyn eframe::App>)
        }),
    )
}

struct About {
    tex: Option<egui::TextureHandle>,
}

impl eframe::App for About {
    fn update(&mut self, ctx: &egui::Context, _f: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                if let Some(t) = &self.tex {
                    ui.add(egui::Image::from_texture(egui::load::SizedTexture::new(
                        t.id(),
                        egui::vec2(96.0, 96.0),
                    )));
                }
                ui.add_space(6.0);
                ui.heading("Veracage");
                ui.label("Encrypted vaults, sandboxed.");
                ui.add_space(4.0);
                ui.label(egui::RichText::new(format!("version {}", env!("CARGO_PKG_VERSION"))).weak());
                ui.add_space(14.0);
                ui.hyperlink_to("GitHub", GITHUB_URL);
                ui.add_space(14.0);
                if ui.button("Close").clicked() {
                    std::process::exit(0);
                }
            });
        });
    }
}
