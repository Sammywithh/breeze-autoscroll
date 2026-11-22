//! Breeze Autoscroll - Click-through Translucent Breeze Overlay Indicator
//!
//! Software-renders the 44x44 KDE Breeze compass anchor with 4x4 supersampling
//! and displays it inside an X11/XWayland 32-bit ARGB override-redirect window
//! with an empty XShape input region so it never steals focus or intercepts clicks.

use std::env;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    AtomEnum, BackingStore, ClipOrdering, ColormapAlloc, ConfigureWindowAux, ConnectionExt as _,
    CreateGCAux, CreateWindowAux, EventMask, ImageFormat, PropMode, StackMode, VisualClass,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;

use crate::input_manager::IndicatorCommand;

pub const INDICATOR_SIZE: usize = 44;
const BASE_DIAMETER: f32 = 32.0;
const SUPERSAMPLE: usize = 4;

#[derive(Clone, Copy)]
struct RgbaF {
    r: f32,
    g: f32,
    b: f32,
    a: f32,
}

impl RgbaF {
    const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self {
            r: (r as f32) / 255.0,
            g: (g as f32) / 255.0,
            b: (b as f32) / 255.0,
            a: (a as f32) / 255.0,
        }
    }

    fn blend_over(self, dst: Self) -> Self {
        let out_a = self.a + dst.a * (1.0 - self.a);
        if out_a <= 1e-6 {
            return Self {
                r: 0.0,
                g: 0.0,
                b: 0.0,
                a: 0.0,
            };
        }
        Self {
            r: (self.r * self.a + dst.r * dst.a * (1.0 - self.a)) / out_a,
            g: (self.g * self.a + dst.g * dst.a * (1.0 - self.a)) / out_a,
            b: (self.b * self.a + dst.b * dst.a * (1.0 - self.a)) / out_a,
            a: out_a,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ArrowMask {
    up: bool,
    down: bool,
    left: bool,
    right: bool,
}

impl ArrowMask {
    fn from_offset(delta_x: f32, delta_y: f32, deadzone: u32, allow_horizontal: bool) -> Self {
        let dz = deadzone as f32;
        if allow_horizontal {
            if delta_x.hypot(delta_y) <= dz {
                return Self {
                    up: false,
                    down: false,
                    left: false,
                    right: false,
                };
            }
            let axis_thresh = dz * 0.4;
            Self {
                up: delta_y < -axis_thresh,
                down: delta_y > axis_thresh,
                left: delta_x < -axis_thresh,
                right: delta_x > axis_thresh,
            }
        } else {
            Self {
                up: delta_y < -dz,
                down: delta_y > dz,
                left: false,
                right: false,
            }
        }
    }
}

/// Renders the 44x44 KDE Breeze compass indicator into premultiplied BGRA bytes
/// (little-endian X11 32-bit ZPixmap order: `[B, G, R, A]`).
#[cfg(test)]
pub fn render_indicator_bgra(
    theme: &str,
    deadzone: u32,
    delta_x: f32,
    delta_y: f32,
    opacity: f32,
    allow_horizontal: bool,
) -> Vec<u8> {
    let arrows = ArrowMask::from_offset(delta_x, delta_y, deadzone, allow_horizontal);
    render_indicator_with_mask(theme, deadzone, arrows, allow_horizontal, opacity)
}

fn render_indicator_with_mask(
    theme: &str,
    deadzone: u32,
    arrows: ArrowMask,
    allow_horizontal: bool,
    opacity: f32,
) -> Vec<u8> {
    let light = theme.eq_ignore_ascii_case("light");
    let opacity = opacity.clamp(0.2, 1.0);

    let bg_color = if light {
        RgbaF::rgba(240, 242, 245, 235)
    } else {
        RgbaF::rgba(30, 34, 38, 235)
    };
    let border_color = if light {
        RgbaF::rgba(190, 195, 200, 220)
    } else {
        RgbaF::rgba(255, 255, 255, 45)
    };
    let glow_color = if light {
        RgbaF::rgba(61, 174, 233, 150)
    } else {
        RgbaF::rgba(61, 174, 233, 140)
    };
    let arrow_inactive = if light {
        RgbaF::rgba(80, 85, 90, 190)
    } else {
        RgbaF::rgba(240, 240, 240, 180)
    };
    let arrow_active = if light {
        RgbaF::rgba(29, 153, 243, 255)
    } else {
        RgbaF::rgba(61, 174, 233, 255)
    };
    let center_dot_color = arrow_active;
    let deadzone_color = if light {
        RgbaF::rgba(160, 165, 170, 80)
    } else {
        RgbaF::rgba(255, 255, 255, 34)
    };

    let center = (INDICATOR_SIZE as f32) * 0.5;
    let radius = BASE_DIAMETER * 0.5; // 16.0 px
    let glow_radius = radius + 5.0; // 21.0 px
    let clamped_dz = (deadzone as f32).clamp(4.0, 14.0);
    let dz_ring_radius = (clamped_dz * 0.7).clamp(4.0, radius - 5.0);
    let arrow_offset = radius * 0.64;
    let arrow_size = 3.6f32;

    let mut out = vec![0u8; INDICATOR_SIZE * INDICATOR_SIZE * 4];
    let inv_ss = 1.0 / (SUPERSAMPLE as f32);
    let ss_samples = (SUPERSAMPLE * SUPERSAMPLE) as f32;

    for py in 0..INDICATOR_SIZE {
        for px in 0..INDICATOR_SIZE {
            let mut acc_r = 0.0f32;
            let mut acc_g = 0.0f32;
            let mut acc_b = 0.0f32;
            let mut acc_a = 0.0f32;

            for sy in 0..SUPERSAMPLE {
                for sx in 0..SUPERSAMPLE {
                    let x = (px as f32) + ((sx as f32) + 0.5) * inv_ss;
                    let y = (py as f32) + ((sy as f32) + 0.5) * inv_ss;
                    let dx = x - center;
                    let dy = y - center;
                    let dist = dx.hypot(dy);

                    let mut sample = RgbaF {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 0.0,
                    };

                    // 1. Subtle Outer Glow
                    if dist <= glow_radius {
                        let t = dist / glow_radius;
                        let glow_alpha = if t <= 0.65 {
                            glow_color.a
                        } else {
                            glow_color.a * (1.0 - (t - 0.65) / 0.35).max(0.0)
                        };
                        sample = RgbaF {
                            r: glow_color.r,
                            g: glow_color.g,
                            b: glow_color.b,
                            a: glow_alpha,
                        }
                        .blend_over(sample);
                    }

                    // 2. Main Glass Circle
                    if dist <= radius {
                        sample = bg_color.blend_over(sample);
                    }

                    // Circle border (1.2 px stroke centered at `radius`)
                    if (dist - radius).abs() <= 0.65 {
                        sample = border_color.blend_over(sample);
                    }

                    // 3. Dotted deadzone guide circle
                    if (dist - dz_ring_radius).abs() <= 0.45 {
                        let angle = dy.atan2(dx);
                        let dash_phase = (angle * dz_ring_radius / 2.2).sin();
                        if dash_phase > 0.0 {
                            sample = deadzone_color.blend_over(sample);
                        }
                    }

                    // 4. Center anchor dot (Breeze blue outer + crisp white core)
                    if dist <= 2.8 {
                        sample = center_dot_color.blend_over(sample);
                    }
                    if dist <= 1.05 {
                        sample = RgbaF::rgba(255, 255, 255, 240).blend_over(sample);
                    }

                    // 5. Directional arrows: 2 arrows (North/South) or 4 arrows (2D mode)
                    let arrow_specs = [
                        (0.0f32, -arrow_offset, 0.0f32, arrows.up, true),
                        (0.0f32, arrow_offset, std::f32::consts::PI, arrows.down, true),
                        (
                            -arrow_offset,
                            0.0f32,
                            -std::f32::consts::FRAC_PI_2,
                            arrows.left,
                            allow_horizontal,
                        ),
                        (
                            arrow_offset,
                            0.0f32,
                            std::f32::consts::FRAC_PI_2,
                            arrows.right,
                            allow_horizontal,
                        ),
                    ];

                    for &(ax, ay, rot, is_active, enabled_axis) in &arrow_specs {
                        if !enabled_axis {
                            continue;
                        }
                        let lx = dx - ax;
                        let ly = dy - ay;
                        // Rotate point into arrow local coordinate space (tip pointing -Y)
                        let (sin_r, cos_r) = (-rot).sin_cos();
                        let rx = lx * cos_r - ly * sin_r;
                        let ry = lx * sin_r + ly * cos_r;

                        let inside = point_in_breeze_arrow(rx, ry, arrow_size);
                        if is_active {
                            if !inside && point_near_breeze_arrow(rx, ry, arrow_size, 1.0) {
                                sample = RgbaF::rgba(61, 174, 233, 140).blend_over(sample);
                            }
                            if inside {
                                sample = arrow_active.blend_over(sample);
                            }
                        } else if inside {
                            sample = arrow_inactive.blend_over(sample);
                        }
                    }

                    // Premultiply sample for box averaging
                    acc_r += sample.r * sample.a;
                    acc_g += sample.g * sample.a;
                    acc_b += sample.b * sample.a;
                    acc_a += sample.a;
                }
            }

            let final_a = ((acc_a / ss_samples) * opacity).clamp(0.0, 1.0);
            let pre_r = ((acc_r / ss_samples) * opacity).clamp(0.0, 1.0);
            let pre_g = ((acc_g / ss_samples) * opacity).clamp(0.0, 1.0);
            let pre_b = ((acc_b / ss_samples) * opacity).clamp(0.0, 1.0);

            let offset = (py * INDICATOR_SIZE + px) * 4;
            out[offset] = (pre_b * 255.0).round() as u8;
            out[offset + 1] = (pre_g * 255.0).round() as u8;
            out[offset + 2] = (pre_r * 255.0).round() as u8;
            out[offset + 3] = (final_a * 255.0).round() as u8;
        }
    }

    out
}

fn arrow_polygon(size: f32) -> [(f32, f32); 4] {
    [
        (0.0, -size * 0.75),
        (-size, size * 0.65),
        (0.0, size * 0.25),
        (size, size * 0.65),
    ]
}

fn point_in_breeze_arrow(px: f32, py: f32, size: f32) -> bool {
    let poly = arrow_polygon(size);
    let mut inside = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (xi, yi) = poly[i];
        let (xj, yj) = poly[j];
        let intersects = ((yi > py) != (yj > py))
            && (px < (xj - xi) * (py - yi) / ((yj - yi) + 1e-9) + xi);
        if intersects {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn point_near_breeze_arrow(px: f32, py: f32, size: f32, margin: f32) -> bool {
    let poly = arrow_polygon(size);
    let margin_sq = margin * margin;
    for i in 0..poly.len() {
        let (ax, ay) = poly[i];
        let (bx, by) = poly[(i + 1) % poly.len()];
        let abx = bx - ax;
        let aby = by - ay;
        let ab_len_sq = abx * abx + aby * aby;
        if ab_len_sq <= 1e-6 {
            continue;
        }
        let t = (((px - ax) * abx + (py - ay) * aby) / ab_len_sq).clamp(0.0, 1.0);
        let cx = ax + abx * t;
        let cy = ay + aby * t;
        let dist_sq = (px - cx) * (px - cx) + (py - cy) * (py - cy);
        if dist_sq <= margin_sq {
            return true;
        }
    }
    false
}

// ============================================================================
// X11 / XWayland Overlay Window Thread
// ============================================================================

struct X11OverlayWindow {
    conn: RustConnection,
    win: u32,
    gc: u32,
    cmap: u32,
    is_mapped: bool,
    theme: String,
    deadzone: u32,
    opacity: f32,
    allow_horizontal: bool,
    last_mask: ArrowMask,
    pixels: Vec<u8>,
}

impl X11OverlayWindow {
    fn new() -> Option<Self> {
        if env::var_os("DISPLAY").is_none() {
            return None;
        }
        let (conn, screen_num) = x11rb::connect(None).ok()?;
        let screen = conn.setup().roots.get(screen_num)?.clone();

        // Find 32-bit TrueColor ARGB visual
        let visual_id = screen
            .allowed_depths
            .iter()
            .find(|d| d.depth == 32)
            .and_then(|d| {
                d.visuals
                    .iter()
                    .find(|v| v.class == VisualClass::TRUE_COLOR)
            })
            .map(|v| v.visual_id)?;

        let cmap = conn.generate_id().ok()?;
        conn.create_colormap(ColormapAlloc::NONE, cmap, screen.root, visual_id)
            .ok()?;

        let win = conn.generate_id().ok()?;
        let size_u16 = INDICATOR_SIZE as u16;
        let aux = CreateWindowAux::new()
            .background_pixel(0)
            .border_pixel(0)
            .backing_store(BackingStore::WHEN_MAPPED)
            .override_redirect(1)
            .colormap(cmap)
            .event_mask(EventMask::EXPOSURE);

        conn.create_window(
            32,
            win,
            screen.root,
            0,
            0,
            size_u16,
            size_u16,
            0,
            WindowClass::INPUT_OUTPUT,
            visual_id,
            &aux,
        )
        .ok()?;

        // Set EWMH hints so KWin/X11 compositor keeps the indicator on top and hidden from taskbar
        set_atom_list_prop(
            &conn,
            win,
            b"_NET_WM_WINDOW_TYPE",
            &[b"_NET_WM_WINDOW_TYPE_TOOLTIP"],
        );
        set_atom_list_prop(
            &conn,
            win,
            b"_NET_WM_STATE",
            &[
                b"_NET_WM_STATE_ABOVE",
                b"_NET_WM_STATE_SKIP_TASKBAR",
                b"_NET_WM_STATE_SKIP_PAGER",
            ],
        );

        // Make window 100% click-through using empty XShape input region
        let _ = conn.shape_rectangles(
            shape::SO::SET,
            shape::SK::INPUT,
            ClipOrdering::UNSORTED,
            win,
            0,
            0,
            &[],
        );

        let gc = conn.generate_id().ok()?;
        conn.create_gc(gc, win, &CreateGCAux::new()).ok()?;
        let _ = conn.flush();

        let initial_mask = ArrowMask {
            up: false,
            down: false,
            left: false,
            right: false,
        };
        let pixels = render_indicator_with_mask("dark", 12, initial_mask, true, 0.92);

        Some(Self {
            conn,
            win,
            gc,
            cmap,
            is_mapped: false,
            theme: "dark".to_string(),
            deadzone: 12,
            opacity: 0.92,
            allow_horizontal: true,
            last_mask: initial_mask,
            pixels,
        })
    }

    fn show(
        &mut self,
        anchor_x: i32,
        anchor_y: i32,
        deadzone: u32,
        theme: String,
        opacity: f32,
        allow_horizontal: bool,
    ) {
        let half = (INDICATOR_SIZE as i32) / 2;
        let x = anchor_x - half;
        let y = anchor_y - half;

        self.theme = theme;
        self.deadzone = deadzone;
        self.opacity = opacity;
        self.allow_horizontal = allow_horizontal;
        self.last_mask = ArrowMask {
            up: false,
            down: false,
            left: false,
            right: false,
        };
        self.pixels = render_indicator_with_mask(
            &self.theme,
            self.deadzone,
            self.last_mask,
            self.allow_horizontal,
            self.opacity,
        );

        let _ = self.conn.configure_window(
            self.win,
            &ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .stack_mode(StackMode::ABOVE),
        );
        if !self.is_mapped {
            let _ = self.conn.map_window(self.win);
            self.is_mapped = true;
        }
        self.upload_pixels();
    }

    fn update_offset(&mut self, delta_x: f32, delta_y: f32) {
        if !self.is_mapped {
            return;
        }
        let next_mask =
            ArrowMask::from_offset(delta_x, delta_y, self.deadzone, self.allow_horizontal);
        if next_mask != self.last_mask {
            self.last_mask = next_mask;
            self.pixels = render_indicator_with_mask(
                &self.theme,
                self.deadzone,
                self.last_mask,
                self.allow_horizontal,
                self.opacity,
            );
            self.upload_pixels();
        }
    }

    fn hide(&mut self) {
        if self.is_mapped {
            let _ = self.conn.unmap_window(self.win);
            let _ = self.conn.flush();
            self.is_mapped = false;
        }
    }

    fn upload_pixels(&self) {
        let size_u16 = INDICATOR_SIZE as u16;
        let _ = self.conn.put_image(
            ImageFormat::Z_PIXMAP,
            self.win,
            self.gc,
            size_u16,
            size_u16,
            0,
            0,
            0,
            32,
            &self.pixels,
        );
        let _ = self.conn.flush();
    }

    fn drain_expose_events(&self) {
        let mut saw_expose = false;
        while let Ok(Some(ev)) = self.conn.poll_for_event() {
            if matches!(ev, x11rb::protocol::Event::Expose(_)) {
                saw_expose = true;
            }
        }
        if saw_expose && self.is_mapped {
            self.upload_pixels();
        }
    }
}

impl Drop for X11OverlayWindow {
    fn drop(&mut self) {
        let _ = self.conn.free_gc(self.gc);
        let _ = self.conn.destroy_window(self.win);
        let _ = self.conn.free_colormap(self.cmap);
        let _ = self.conn.flush();
    }
}

fn set_atom_list_prop(conn: &RustConnection, win: u32, prop_name: &[u8], atom_names: &[&[u8]]) {
    let Ok(prop_cookie) = conn.intern_atom(false, prop_name) else {
        return;
    };
    let Ok(prop_reply) = prop_cookie.reply() else {
        return;
    };

    let mut atoms = Vec::with_capacity(atom_names.len());
    for &name in atom_names {
        if let Ok(c) = conn.intern_atom(false, name)
            && let Ok(r) = c.reply()
        {
            atoms.push(r.atom);
        }
    }
    if atoms.is_empty() {
        return;
    }

    let bytes: Vec<u8> = atoms.iter().flat_map(|a| a.to_ne_bytes()).collect();
    let _ = conn.change_property(
        PropMode::REPLACE,
        win,
        prop_reply.atom,
        AtomEnum::ATOM,
        32,
        atoms.len() as u32,
        &bytes,
    );
}

/// Manages the background indicator rendering & X11 window thread.
pub struct IndicatorHandle {
    tx: Sender<IndicatorCommand>,
    handle: Option<JoinHandle<()>>,
}

impl IndicatorHandle {
    pub fn spawn() -> (Self, Sender<IndicatorCommand>) {
        let (tx, rx) = mpsc::channel();
        let tx_clone = tx.clone();
        let handle = thread::Builder::new()
            .name("breeze-indicator".to_string())
            .spawn(move || run_indicator_loop(rx))
            .ok();
        (Self { tx, handle }, tx_clone)
    }

    pub fn stop(&mut self) {
        let _ = self.tx.send(IndicatorCommand::Shutdown);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for IndicatorHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_indicator_loop(rx: Receiver<IndicatorCommand>) {
    let mut overlay = X11OverlayWindow::new();

    while let Ok(cmd) = rx.recv() {
        // Coalesce burst offset updates while preserving Show / Hide / Shutdown order
        match cmd {
            IndicatorCommand::Show {
                anchor_x,
                anchor_y,
                deadzone,
                theme,
                opacity,
                allow_horizontal,
            } => {
                if overlay.is_none() {
                    overlay = X11OverlayWindow::new();
                }
                if let Some(ov) = overlay.as_mut() {
                    ov.show(
                        anchor_x,
                        anchor_y,
                        deadzone,
                        theme,
                        opacity,
                        allow_horizontal,
                    );
                }
            }
            IndicatorCommand::UpdateOffset { delta_x, delta_y } => {
                if let Some(ov) = overlay.as_mut() {
                    ov.update_offset(delta_x, delta_y);
                    ov.drain_expose_events();
                }
            }
            IndicatorCommand::Hide => {
                if let Some(ov) = overlay.as_mut() {
                    ov.hide();
                }
            }
            IndicatorCommand::Shutdown => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_44x44_indicator_with_transparent_corners_and_opaque_center() {
        let dark = render_indicator_bgra("dark", 12, 0.0, 0.0, 0.92, true);
        assert_eq!(dark.len(), INDICATOR_SIZE * INDICATOR_SIZE * 4);

        // Corner (0, 0) must be completely transparent
        assert_eq!(dark[3], 0);

        // Center (22, 22) must be near-opaque white/blue dot
        let center_idx = (22 * INDICATOR_SIZE + 22) * 4;
        assert!(dark[center_idx + 3] > 200);

        // Light theme renders distinct colors from dark theme
        let light = render_indicator_bgra("light", 12, 0.0, 0.0, 0.92, true);
        assert_ne!(dark, light);
    }

    #[test]
    fn renders_two_arrows_when_horizontal_disabled_and_four_when_enabled() {
        let two_way = render_indicator_bgra("dark", 12, 0.0, 0.0, 0.92, false);
        let four_way = render_indicator_bgra("dark", 12, 0.0, 0.0, 0.92, true);
        assert_ne!(two_way, four_way);

        // Sideways offset in 2-way mode must not light up left/right arrows
        let two_way_sideways = render_indicator_bgra("dark", 12, 40.0, 0.0, 0.92, false);
        assert_eq!(two_way, two_way_sideways);
    }

    #[test]
    fn highlights_active_and_diagonal_direction_arrows() {
        let idle = render_indicator_bgra("dark", 12, 0.0, 0.0, 0.92, true);
        let scrolling_up = render_indicator_bgra("dark", 12, 0.0, -30.0, 0.92, true);
        let diagonal = render_indicator_bgra("dark", 12, 30.0, -30.0, 0.92, true);
        assert_ne!(idle, scrolling_up);
        assert_ne!(scrolling_up, diagonal);

        let mask = ArrowMask::from_offset(30.0, -30.0, 12, true);
        assert!(mask.up);
        assert!(mask.right);
        assert!(!mask.down);
        assert!(!mask.left);
    }
}
