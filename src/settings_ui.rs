//! KDE Plasma 6 Breeze styled Settings & About UI built with `eframe`/`egui`.

use std::env;
use std::fs;
use std::sync::Arc;

use eframe::egui::{
    self, Color32, ComboBox, CornerRadius, CursorIcon, FontData, FontDefinitions, FontFamily,
    FontId, Frame, Margin, RichText, Slider, Stroke, TextEdit, Vec2, ViewportBuilder,
    ViewportCommand, X11WindowType,
};

use crate::config::{ConfigData, ConfigManager};
use crate::tray::{notify_daemon_reload_config, render_tray_icon_argb32_be};

const BREEZE_BLUE: Color32 = Color32::from_rgb(0x3d, 0xae, 0xe9);
const BREEZE_BLUE_HOVER: Color32 = Color32::from_rgb(0x1d, 0x99, 0xf3);

#[derive(Clone, Copy)]
struct SettingsPalette {
    dark: bool,
    bg_dialog: Color32,
    bg_card: Color32,
    border_card: Color32,
    bg_widget: Color32,
    border_widget: Color32,
    text_primary: Color32,
    text_secondary: Color32,
    accent: Color32,
}

impl SettingsPalette {
    const fn for_theme(theme: &str) -> Self {
        if matches_light(theme) {
            Self {
                dark: false,
                bg_dialog: Color32::from_rgb(0xef, 0xf0, 0xf1),
                bg_card: Color32::from_rgb(0xfc, 0xfc, 0xfc),
                border_card: Color32::from_rgb(0xd4, 0xd7, 0xda),
                bg_widget: Color32::from_rgb(0xe4, 0xe6, 0xe8),
                border_widget: Color32::from_rgb(0xbd, 0xc2, 0xc7),
                text_primary: Color32::from_rgb(0x23, 0x26, 0x29),
                text_secondary: Color32::from_rgb(0x60, 0x65, 0x6b),
                accent: BREEZE_BLUE_HOVER,
            }
        } else {
            Self {
                dark: true,
                bg_dialog: Color32::from_rgb(0x24, 0x27, 0x2a),
                bg_card: Color32::from_rgb(0x2a, 0x2e, 0x32),
                border_card: Color32::from_rgb(0x36, 0x3b, 0x40),
                bg_widget: Color32::from_rgb(0x31, 0x36, 0x3b),
                border_widget: Color32::from_rgb(0x45, 0x4c, 0x53),
                text_primary: Color32::from_rgb(0xef, 0xf0, 0xf1),
                text_secondary: Color32::from_rgb(0x8c, 0x93, 0x98),
                accent: BREEZE_BLUE,
            }
        }
    }
}

const fn matches_light(theme: &str) -> bool {
    let bytes = theme.as_bytes();
    bytes.len() == 5
        && (bytes[0] | 32) == b'l'
        && (bytes[1] | 32) == b'i'
        && (bytes[2] | 32) == b'g'
        && (bytes[3] | 32) == b'h'
        && (bytes[4] | 32) == b't'
}

fn configure_display_backend() {
    if env::var_os("WINIT_UNIX_BACKEND").is_none()
        && env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
    {
        unsafe {
            env::set_var("WINIT_UNIX_BACKEND", "x11");
        }
    }
}

fn configure_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    let regular_candidates = [
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/truetype/ubuntu/Ubuntu-R.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    ];
    if let Some(bytes) = regular_candidates.iter().find_map(|p| fs::read(p).ok()) {
        fonts.font_data.insert(
            "system_regular".to_owned(),
            Arc::new(FontData::from_owned(bytes)),
        );
        if let Some(family) = fonts.families.get_mut(&FontFamily::Proportional) {
            family.insert(0, "system_regular".to_owned());
        }
    }
    ctx.set_fonts(fonts);
}

fn apply_breeze_visuals(ctx: &egui::Context, palette: SettingsPalette) {
    let mut visuals = if palette.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    visuals.panel_fill = palette.bg_dialog;
    visuals.window_fill = palette.bg_dialog;
    visuals.selection.bg_fill = palette.accent;
    visuals.selection.stroke = Stroke::new(1.0, Color32::WHITE);
    visuals.widgets.inactive.bg_fill = palette.bg_widget;
    visuals.widgets.inactive.weak_bg_fill = palette.bg_widget;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, palette.border_widget);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, palette.text_primary);
    visuals.widgets.hovered.bg_fill = palette.bg_widget;
    visuals.widgets.hovered.weak_bg_fill = palette.bg_widget;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.5, palette.accent);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, palette.text_primary);
    visuals.widgets.active.bg_fill = palette.accent;
    visuals.widgets.active.weak_bg_fill = palette.accent;
    visuals.widgets.active.bg_stroke = Stroke::new(1.5, palette.accent);
    visuals.widgets.active.fg_stroke = Stroke::new(1.5, Color32::WHITE);
    ctx.set_visuals(visuals);
}

fn build_app_icon() -> Option<egui::IconData> {
    let argb = render_tray_icon_argb32_be(true, 64);
    let mut rgba = Vec::with_capacity(argb.len());
    for px in argb.chunks_exact(4) {
        rgba.extend_from_slice(&[px[1], px[2], px[3], px[0]]);
    }
    Some(egui::IconData {
        rgba,
        width: 64,
        height: 64,
    })
}

struct SettingsApp {
    config_mgr: ConfigManager,
    draft: ConfigData,
    excluded_csv: String,
    confirm_reset: bool,
    last_theme: String,
}

impl SettingsApp {
    fn new(cc: &eframe::CreationContext<'_>, config_mgr: ConfigManager) -> Self {
        configure_fonts(&cc.egui_ctx);
        let draft = config_mgr.snapshot();
        let excluded_csv = draft.excluded_as_csv();
        let palette = SettingsPalette::for_theme(&draft.theme);
        apply_breeze_visuals(&cc.egui_ctx, palette);
        let last_theme = draft.theme.clone();

        Self {
            config_mgr,
            draft,
            excluded_csv,
            confirm_reset: false,
            last_theme,
        }
    }

    fn persist_draft(&mut self) {
        self.draft.set_excluded_from_csv(&self.excluded_csv);
        let updated = self.draft.clone();
        self.config_mgr.update(true, |cfg| {
            *cfg = updated;
        });
        notify_daemon_reload_config();
    }
}

fn draw_group_card<F>(ui: &mut egui::Ui, palette: SettingsPalette, title: &str, add: F)
where
    F: FnOnce(&mut egui::Ui),
{
    Frame::NONE
        .fill(palette.bg_card)
        .stroke(Stroke::new(1.0, palette.border_card))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(14, 10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(
                RichText::new(title)
                    .size(13.0)
                    .strong()
                    .color(palette.accent),
            );
            ui.add_space(6.0);
            add(ui);
        });
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.draft.theme != self.last_theme {
            apply_breeze_visuals(&ctx, SettingsPalette::for_theme(&self.draft.theme));
            self.last_theme = self.draft.theme.clone();
        }
        let palette = SettingsPalette::for_theme(&self.draft.theme);
        let mut changed = false;

        egui::CentralPanel::default()
            .frame(
                Frame::NONE
                    .fill(palette.bg_dialog)
                    .inner_margin(Margin::symmetric(18, 16)),
            )
            .show(ui, |ui| {
                // Header
                ui.label(
                    RichText::new("Mausrad-Klick Autoscroll")
                        .size(19.0)
                        .strong()
                        .color(if palette.dark {
                            Color32::WHITE
                        } else {
                            palette.text_primary
                        }),
                );
                ui.add_space(2.0);
                ui.label(
                    RichText::new(
                        "Klassisches Windows-Mittelklick-Scrollen im Kubuntu 26.04 Breeze-Stil (Rust)",
                    )
                    .size(12.0)
                    .color(palette.text_secondary),
                );
                ui.add_space(12.0);

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .max_height((ui.available_height() - 48.0).max(200.0))
                    .show(ui, |ui| {
                        // 1. Allgemein
                        draw_group_card(ui, palette, "Allgemein", |ui| {
                            changed |= ui
                                .checkbox(
                                    &mut self.draft.enabled,
                                    "Autoscroll-Funktion aktivieren",
                                )
                                .changed();
                            ui.add_space(2.0);
                            changed |= ui
                                .checkbox(
                                    &mut self.draft.autostart,
                                    "Automatisch beim Anmelden starten (KDE Autostart)",
                                )
                                .changed();
                        });

                        ui.add_space(10.0);

                        // 2. Mausrad-Klick Verhalten
                        draw_group_card(ui, palette, "Mausrad-Klick Verhalten", |ui| {
                            if ui
                                .radio(
                                    self.draft.mode == "smart",
                                    "Windows-Automatik: Einrast- & Halte-Modus (Empfohlen)",
                                )
                                .on_hover_text(
                                    "Klick & Loslassen fixiert den Anker dauerhaft (Einrast-Modus, Stopp per Klick/Esc). Drücken & Ziehen aus der Totzone scrollt nur bis zum Loslassen (Halte-Modus).",
                                )
                                .clicked()
                            {
                                self.draft.mode = "smart".to_string();
                                changed = true;
                            }
                            if ui
                                .radio(
                                    self.draft.mode == "toggle",
                                    "Immer Einrast-Modus (Toggle)",
                                )
                                .on_hover_text(
                                    "Mittelklick fixiert den Anker unabhängig von der Bewegung. Beenden durch erneuten Klick oder Esc.",
                                )
                                .clicked()
                            {
                                self.draft.mode = "toggle".to_string();
                                changed = true;
                            }
                            if ui
                                .radio(
                                    self.draft.mode == "drag",
                                    "Immer Halte-Modus (Drag)",
                                )
                                .on_hover_text(
                                    "Autoscroll ist nur aktiv, solange das Mausrad gedrückt gehalten wird.",
                                )
                                .clicked()
                            {
                                self.draft.mode = "drag".to_string();
                                changed = true;
                            }

                            ui.add_space(4.0);
                            changed |= ui
                                .checkbox(
                                    &mut self.draft.allow_horizontal,
                                    "Horizontales 2D-Scrollen erlauben (4-Wege-Anker inkl. Diagonal)",
                                )
                                .on_hover_text(
                                    "Aktiv: 4-Wege-Anker (Hoch/Runter/Links/Rechts + Diagonal). Deaktiviert: 2-Wege-Anker (nur Vertikal).",
                                )
                                .changed();

                            ui.add_space(2.0);
                            changed |= ui
                                .checkbox(
                                    &mut self.draft.short_click_passthrough,
                                    "Kurze Mittelklicks systemweit durchreichen (Tabs, Links, Ordner in allen Apps)",
                                )
                                .on_hover_text(
                                    "Systemweiter Schutz für alle Programme (Firefox, Obsidian, Nautilus, Dolphin, VS Code usw.): Kurzes Antippen ohne Ziehen bleibt ein normaler Mittelklick. Direktes Ziehen startet sofort den Halte-Modus; kurzes Halten rastet den Anker ein.",
                                )
                                .changed();

                            if self.draft.short_click_passthrough {
                                ui.horizontal(|ui| {
                                    ui.add_space(22.0);
                                    ui.label(
                                        RichText::new("Haltezeit für Einrast-Modus:")
                                            .size(12.0)
                                            .color(palette.text_secondary),
                                    );
                                    let mut delay_ms = self.draft.hold_delay_ms as i32;
                                    if ui
                                        .add(
                                            Slider::new(&mut delay_ms, 100..=400)
                                                .show_value(false),
                                        )
                                        .changed()
                                    {
                                        self.draft.hold_delay_ms = delay_ms as u64;
                                        changed = true;
                                    }
                                    ui.label(
                                        RichText::new(format!("{} ms", self.draft.hold_delay_ms))
                                            .strong()
                                            .color(palette.accent),
                                    );
                                });
                            }

                            ui.add_space(2.0);
                            changed |= ui
                                .checkbox(
                                    &mut self.draft.ignore_hyperlinks_and_tabs,
                                    "Zusätzliche Erkennung interaktiver Elemente (Cursor, Fensterleisten & AT-SPI2)",
                                )
                                .on_hover_text(
                                    "Unterdrückt Autoscroll zusätzlich auch bei längerem Gedrückthalten über Hand-Cursor-Links, Titelleisten/Tableisten und AT-SPI2-UI-Elementen.",
                                )
                                .changed();
                        });

                        ui.add_space(10.0);

                        // 3. 3D / CAD & Slicer Schutz
                        draw_group_card(
                            ui,
                            palette,
                            "3D-Raum, CAD & Slicer Schutz (Onshape, Bambu Studio)",
                            |ui| {
                                changed |= ui
                                    .checkbox(
                                        &mut self.draft.auto_exclude_3d_apps,
                                        "In 3D-Programmen & 3D-Websites automatisch deaktivieren",
                                    )
                                    .on_hover_text(
                                        "Erkennt Fenster unter dem Zeiger sowie aktive Browser-Tabs (z. B. Onshape, Bambu Studio, OrcaSlicer, FreeCAD, Blender) und lässt dort natives 3D-Drehen unberührt.",
                                    )
                                    .changed();

                                ui.add_space(2.0);
                                changed |= ui
                                    .checkbox(
                                        &mut self.draft.protect_3d_drag,
                                        "Strenger Orbit-Schutz: Bei sofortiger Bewegung nicht auslösen (deaktiviert Halte-Modus)",
                                    )
                                    .on_hover_text(
                                        "Optional für nicht gelistete 3D-Websites: Bricht Autoscroll ab, wenn die Maus beim Gedrückthalten sofort bewegt wird. Hinweis: Deaktiviert den Windows-Halte-Modus (Drücken, Ziehen, Loslassen).",
                                    )
                                    .changed();

                                ui.add_space(6.0);
                                ui.label(
                                    RichText::new("Ausgeschlossene Apps & Fenstertitel-Keywords (kommagetrennt):")
                                        .size(11.5)
                                        .color(palette.text_secondary),
                                );
                                ui.add_space(3.0);
                                let edit_resp = ui.add_enabled(
                                    self.draft.auto_exclude_3d_apps,
                                    TextEdit::multiline(&mut self.excluded_csv)
                                        .desired_rows(2)
                                        .desired_width(f32::INFINITY)
                                        .font(FontId::new(11.5, FontFamily::Proportional)),
                                );
                                if edit_resp.changed() {
                                    changed = true;
                                }
                            },
                        );

                        ui.add_space(10.0);

                        // 4. Geschwindigkeit & Empfindlichkeit
                        draw_group_card(
                            ui,
                            palette,
                            "Geschwindigkeit & Empfindlichkeit",
                            |ui| {
                                let mut speed_pct =
                                    (self.draft.scroll_speed * 100.0).round() as i32;
                                ui.horizontal(|ui| {
                                    ui.label("Scroll-Geschwindigkeit:");
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(format!("{speed_pct}%"))
                                                    .strong()
                                                    .color(palette.accent),
                                            );
                                            if ui
                                                .add(
                                                    Slider::new(&mut speed_pct, 20..=300)
                                                        .show_value(false),
                                                )
                                                .changed()
                                            {
                                                self.draft.scroll_speed =
                                                    (speed_pct as f32) / 100.0;
                                                changed = true;
                                            }
                                        },
                                    );
                                });

                                ui.add_space(4.0);
                                let mut dz = self.draft.deadzone as i32;
                                ui.horizontal(|ui| {
                                    ui.label("Neutralbereich (Totzone):");
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(format!("{dz} px"))
                                                    .strong()
                                                    .color(palette.accent),
                                            );
                                            if ui
                                                .add(Slider::new(&mut dz, 4..=30).show_value(false))
                                                .changed()
                                            {
                                                self.draft.deadzone = dz as u32;
                                                changed = true;
                                            }
                                        },
                                    );
                                });
                            },
                        );

                        ui.add_space(10.0);

                        // 5. Erscheinungsbild
                        draw_group_card(ui, palette, "Erscheinungsbild", |ui| {
                            ui.horizontal(|ui| {
                                ui.label("Design-Farbschema:");
                                ui.add_space(8.0);
                                let selected_label = if matches_light(&self.draft.theme) {
                                    "KDE Breeze Hell"
                                } else {
                                    "KDE Breeze Dunkel (Standard)"
                                };
                                ComboBox::from_id_salt("theme_selector")
                                    .selected_text(selected_label)
                                    .width(230.0)
                                    .show_ui(ui, |ui| {
                                        if ui
                                            .selectable_value(
                                                &mut self.draft.theme,
                                                "dark".to_string(),
                                                "KDE Breeze Dunkel (Standard)",
                                            )
                                            .changed()
                                        {
                                            changed = true;
                                        }
                                        if ui
                                            .selectable_value(
                                                &mut self.draft.theme,
                                                "light".to_string(),
                                                "KDE Breeze Hell",
                                            )
                                            .changed()
                                        {
                                            changed = true;
                                        }
                                    });
                            });
                        });
                    });

                ui.add_space(12.0);

                // Footer action buttons
                ui.horizontal(|ui| {
                    if !self.confirm_reset {
                        if ui
                            .add_sized(
                                Vec2::new(124.0, 32.0),
                                egui::Button::new("Standardwerte")
                                    .fill(palette.bg_widget)
                                    .corner_radius(CornerRadius::same(6)),
                            )
                            .on_hover_cursor(CursorIcon::PointingHand)
                            .clicked()
                        {
                            self.confirm_reset = true;
                        }
                    } else {
                        ui.label(
                            RichText::new("Zurücksetzen?")
                                .size(12.0)
                                .color(palette.text_secondary),
                        );
                        if ui
                            .add_sized(
                                Vec2::new(56.0, 30.0),
                                egui::Button::new("Ja")
                                    .fill(palette.accent)
                                    .corner_radius(CornerRadius::same(6)),
                            )
                            .clicked()
                        {
                            self.config_mgr.reset_defaults();
                            self.draft = self.config_mgr.snapshot();
                            self.excluded_csv = self.draft.excluded_as_csv();
                            self.confirm_reset = false;
                            notify_daemon_reload_config();
                        }
                        if ui
                            .add_sized(
                                Vec2::new(64.0, 30.0),
                                egui::Button::new("Nein")
                                    .fill(palette.bg_widget)
                                    .corner_radius(CornerRadius::same(6)),
                            )
                            .clicked()
                        {
                            self.confirm_reset = false;
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let close_btn = egui::Button::new(
                            RichText::new("Schließen").strong().color(Color32::WHITE),
                        )
                        .fill(palette.accent)
                        .stroke(Stroke::new(1.0, palette.accent))
                        .corner_radius(CornerRadius::same(6));
                        if ui
                            .add_sized(Vec2::new(112.0, 32.0), close_btn)
                            .on_hover_cursor(CursorIcon::PointingHand)
                            .clicked()
                        {
                            ctx.send_viewport_cmd(ViewportCommand::Close);
                        }
                    });
                });
            });

        if changed {
            self.persist_draft();
        }
    }
}

struct AboutApp {
    palette: SettingsPalette,
}

impl AboutApp {
    fn new(cc: &eframe::CreationContext<'_>, theme: &str) -> Self {
        configure_fonts(&cc.egui_ctx);
        let palette = SettingsPalette::for_theme(theme);
        apply_breeze_visuals(&cc.egui_ctx, palette);
        Self { palette }
    }
}

impl eframe::App for AboutApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let p = self.palette;
        egui::CentralPanel::default()
            .frame(
                Frame::NONE
                    .fill(p.bg_dialog)
                    .inner_margin(Margin::symmetric(20, 18)),
            )
            .show(ui, |ui| {
                ui.label(
                    RichText::new(format!(
                        "Breeze Autoscroll v{} (Rust)",
                        env!("CARGO_PKG_VERSION")
                    ))
                    .size(18.0)
                    .strong()
                    .color(p.accent),
                );
                ui.add_space(6.0);
                ui.label(
                    RichText::new(
                        "Klassische Windows-Mausrad-Klick-Autoscroll-Funktion für Kubuntu 26.04 (KDE Plasma 6 / Wayland & X11).",
                    )
                    .color(p.text_primary),
                );
                ui.add_space(10.0);
                for item in [
                    "• Flüssiges Hi-Res 2D-Scrollen via Linux kernel uinput",
                    "• Zweistufiger 3D/CAD-Schutz für Onshape, Bambu Studio, OrcaSlicer, FreeCAD & Blender",
                    "• Transluzenter KDE Breeze Overlay-Indikator (klick-durchlässig)",
                    "• Natives KDE Plasma 6 System-Tray-Applet",
                ] {
                    ui.label(RichText::new(item).size(12.5).color(p.text_secondary));
                }
                ui.add_space(16.0);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let ok_btn = egui::Button::new(
                        RichText::new("OK").strong().color(Color32::WHITE),
                    )
                    .fill(p.accent)
                    .corner_radius(CornerRadius::same(6));
                    if ui
                        .add_sized(Vec2::new(96.0, 30.0), ok_btn)
                        .on_hover_cursor(CursorIcon::PointingHand)
                        .clicked()
                    {
                        ctx.send_viewport_cmd(ViewportCommand::Close);
                    }
                });
            });
    }
}

/// Opens the Breeze Settings window as a standalone GUI process.
pub fn run_settings_window(config_mgr: ConfigManager) -> Result<(), eframe::Error> {
    configure_display_backend();

    let mut viewport = ViewportBuilder::default()
        .with_title("Breeze Autoscroll - Einstellungen")
        .with_app_id("breeze-autoscroll")
        .with_inner_size([590.0, 720.0])
        .with_min_inner_size([520.0, 620.0])
        .with_window_type(X11WindowType::Dialog);

    if let Some(icon) = build_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "breeze-autoscroll-settings",
        options,
        Box::new(move |cc| Ok(Box::new(SettingsApp::new(cc, config_mgr)))),
    )
}

/// Opens the Breeze About dialog.
pub fn run_about_window(config_mgr: ConfigManager) -> Result<(), eframe::Error> {
    configure_display_backend();
    let theme = config_mgr.snapshot().theme;

    let mut viewport = ViewportBuilder::default()
        .with_title("Über Breeze Autoscroll")
        .with_app_id("breeze-autoscroll")
        .with_inner_size([480.0, 250.0])
        .with_resizable(false)
        .with_window_type(X11WindowType::Dialog);

    if let Some(icon) = build_app_icon() {
        viewport = viewport.with_icon(icon);
    }

    let options = eframe::NativeOptions {
        viewport,
        centered: true,
        ..Default::default()
    };

    eframe::run_native(
        "breeze-autoscroll-about",
        options,
        Box::new(move |cc| Ok(Box::new(AboutApp::new(cc, &theme)))),
    )
}
