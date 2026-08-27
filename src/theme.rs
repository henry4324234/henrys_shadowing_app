//! The app's visual identity, now in switchable flavors: a `Palette` holds
//! every color the UI uses, several named schemes are defined below, and the
//! Theme dropdown in Settings swaps between them live. `apply` restyles the
//! egui context from whichever palette is current.
//!
//! Everything color-related lives here so the rest of the code never
//! hard-codes an RGB value — widgets ask `palette()` for their colors.

use std::sync::atomic::{AtomicUsize, Ordering};

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle,
};

/// Every color the UI uses, as one named scheme.
#[derive(Clone, Copy)]
pub struct Palette {
    /// Shown in the Theme dropdown, and saved in the config file.
    pub name: &'static str,
    /// Drives the base egui visuals and the OS window chrome.
    pub dark: bool,

    /// Window / panel background.
    pub bg: Color32,
    /// Raised surfaces: cards, windows, menus.
    pub card: Color32,
    /// 1px outline around raised surfaces.
    pub card_stroke: Color32,
    /// Sunken surfaces: text fields, progress troughs.
    pub field: Color32,

    /// Interactive widget fills (buttons, combos, drag values).
    pub widget: Color32,
    pub widget_hover: Color32,
    pub widget_active: Color32,
    pub widget_stroke: Color32,

    /// Text.
    pub text: Color32,
    pub text_strong: Color32,
    pub text_muted: Color32,
    pub text_faint: Color32,

    /// The working accent: progress bars, focus rings, the "running" state.
    pub accent: Color32,

    /// Call-to-action buttons (Start all, Download), with their text color.
    pub primary: Color32,
    pub primary_hover: Color32,
    pub primary_active: Color32,
    pub on_primary: Color32,

    /// Status colors.
    pub success: Color32,
    pub error: Color32,
    pub info: Color32,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color32 {
    Color32::from_rgb(r, g, b)
}

/// The original look: deep charcoal-blue, amber accent, light-blue actions.
const MIDNIGHT_AMBER: Palette = Palette {
    name: "Midnight Amber",
    dark: true,
    bg: rgb(0x0F, 0x13, 0x18),
    card: rgb(0x17, 0x1C, 0x23),
    card_stroke: rgb(0x24, 0x2B, 0x35),
    field: rgb(0x0A, 0x0D, 0x11),
    widget: rgb(0x22, 0x2A, 0x34),
    widget_hover: rgb(0x2C, 0x36, 0x42),
    widget_active: rgb(0x35, 0x41, 0x50),
    widget_stroke: rgb(0x2E, 0x38, 0x44),
    text: rgb(0xE6, 0xEB, 0xF0),
    text_strong: rgb(0xFA, 0xFC, 0xFE),
    text_muted: rgb(0x8E, 0x9A, 0xA7),
    text_faint: rgb(0x5E, 0x69, 0x74),
    accent: rgb(0xF6, 0xAD, 0x3C),
    primary: rgb(0x7E, 0xC3, 0xF7),
    primary_hover: rgb(0x9C, 0xD2, 0xFA),
    primary_active: rgb(0x66, 0xAF, 0xE4),
    on_primary: rgb(0x0A, 0x1C, 0x2B),
    success: rgb(0x4C, 0xC3, 0x8A),
    error: rgb(0xF1, 0x6A, 0x6A),
    info: rgb(0x6C, 0xB2, 0xF5),
};

/// Indigo night with soft neon pastels, after the VS Code theme.
const TOKYO_NIGHT: Palette = Palette {
    name: "Tokyo Night",
    dark: true,
    bg: rgb(0x16, 0x16, 0x1E),
    card: rgb(0x1F, 0x23, 0x35),
    card_stroke: rgb(0x2C, 0x31, 0x49),
    field: rgb(0x12, 0x12, 0x19),
    widget: rgb(0x29, 0x2E, 0x44),
    widget_hover: rgb(0x34, 0x3A, 0x56),
    widget_active: rgb(0x3E, 0x45, 0x68),
    widget_stroke: rgb(0x36, 0x3C, 0x58),
    text: rgb(0xC0, 0xCA, 0xF5),
    text_strong: rgb(0xE4, 0xEA, 0xFD),
    text_muted: rgb(0x81, 0x89, 0xB0),
    text_faint: rgb(0x56, 0x5F, 0x89),
    accent: rgb(0xFF, 0x9E, 0x64),
    primary: rgb(0x7A, 0xA2, 0xF7),
    primary_hover: rgb(0x94, 0xB5, 0xF9),
    primary_active: rgb(0x64, 0x89, 0xD8),
    on_primary: rgb(0x0D, 0x12, 0x20),
    success: rgb(0x9E, 0xCE, 0x6A),
    error: rgb(0xF7, 0x76, 0x8E),
    info: rgb(0x7D, 0xCF, 0xFF),
};

/// Calm arctic blue-grays, after the Nord palette.
const NORD: Palette = Palette {
    name: "Nord",
    dark: true,
    bg: rgb(0x24, 0x29, 0x33),
    card: rgb(0x2E, 0x34, 0x40),
    card_stroke: rgb(0x3B, 0x42, 0x52),
    field: rgb(0x1E, 0x23, 0x2D),
    widget: rgb(0x3B, 0x42, 0x52),
    widget_hover: rgb(0x43, 0x4C, 0x5E),
    widget_active: rgb(0x4C, 0x56, 0x6A),
    widget_stroke: rgb(0x43, 0x4C, 0x5E),
    text: rgb(0xE5, 0xE9, 0xF0),
    text_strong: rgb(0xEC, 0xEF, 0xF4),
    text_muted: rgb(0x9A, 0xA5, 0xB8),
    text_faint: rgb(0x6C, 0x76, 0x89),
    accent: rgb(0xEB, 0xCB, 0x8B),
    primary: rgb(0x88, 0xC0, 0xD0),
    primary_hover: rgb(0xA3, 0xD3, 0xE0),
    primary_active: rgb(0x6F, 0xA8, 0xBA),
    on_primary: rgb(0x10, 0x18, 0x1D),
    success: rgb(0xA3, 0xBE, 0x8C),
    error: rgb(0xBF, 0x61, 0x6A),
    info: rgb(0x81, 0xA1, 0xC1),
};

/// High-contrast purple and candy colors, after the Dracula theme.
const DRACULA: Palette = Palette {
    name: "Dracula",
    dark: true,
    bg: rgb(0x1E, 0x1F, 0x29),
    card: rgb(0x28, 0x2A, 0x36),
    card_stroke: rgb(0x3A, 0x3D, 0x4F),
    field: rgb(0x19, 0x1A, 0x23),
    widget: rgb(0x34, 0x37, 0x47),
    widget_hover: rgb(0x41, 0x44, 0x59),
    widget_active: rgb(0x4C, 0x50, 0x69),
    widget_stroke: rgb(0x3F, 0x42, 0x57),
    text: rgb(0xF8, 0xF8, 0xF2),
    text_strong: rgb(0xFF, 0xFF, 0xFF),
    text_muted: rgb(0xA0, 0xA8, 0xCE),
    text_faint: rgb(0x62, 0x72, 0xA4),
    accent: rgb(0xFF, 0xB8, 0x6C),
    primary: rgb(0xBD, 0x93, 0xF9),
    primary_hover: rgb(0xCF, 0xAE, 0xFB),
    primary_active: rgb(0xA3, 0x79, 0xE8),
    on_primary: rgb(0x19, 0x0D, 0x2A),
    success: rgb(0x50, 0xFA, 0x7B),
    error: rgb(0xFF, 0x55, 0x55),
    info: rgb(0x8B, 0xE9, 0xFD),
};

/// Muted greens and warm parchment text, after Everforest.
const EVERFOREST: Palette = Palette {
    name: "Everforest",
    dark: true,
    bg: rgb(0x1E, 0x23, 0x26),
    card: rgb(0x27, 0x2E, 0x33),
    card_stroke: rgb(0x37, 0x41, 0x45),
    field: rgb(0x19, 0x1E, 0x21),
    widget: rgb(0x34, 0x3F, 0x44),
    widget_hover: rgb(0x3D, 0x48, 0x4D),
    widget_active: rgb(0x47, 0x52, 0x58),
    widget_stroke: rgb(0x41, 0x4B, 0x50),
    text: rgb(0xD3, 0xC6, 0xAA),
    text_strong: rgb(0xE6, 0xDC, 0xC3),
    text_muted: rgb(0x9D, 0xA9, 0xA0),
    text_faint: rgb(0x7A, 0x84, 0x78),
    accent: rgb(0xDB, 0xBC, 0x7F),
    primary: rgb(0x7F, 0xBB, 0xB3),
    primary_hover: rgb(0x9C, 0xCD, 0xC6),
    primary_active: rgb(0x66, 0xA2, 0x9A),
    on_primary: rgb(0x10, 0x20, 0x1D),
    success: rgb(0xA7, 0xC0, 0x80),
    error: rgb(0xE6, 0x7E, 0x80),
    info: rgb(0x7F, 0xBB, 0xB3),
};

/// The one light option: white cards on cool paper gray.
const PAPER: Palette = Palette {
    name: "Paper (light)",
    dark: false,
    bg: rgb(0xF2, 0xF4, 0xF7),
    card: rgb(0xFF, 0xFF, 0xFF),
    card_stroke: rgb(0xDF, 0xE3, 0xEA),
    field: rgb(0xEB, 0xEE, 0xF3),
    widget: rgb(0xE5, 0xE9, 0xF0),
    widget_hover: rgb(0xD8, 0xDE, 0xE8),
    widget_active: rgb(0xCB, 0xD3, 0xE0),
    widget_stroke: rgb(0xD5, 0xDB, 0xE5),
    text: rgb(0x2A, 0x32, 0x40),
    text_strong: rgb(0x11, 0x17, 0x22),
    text_muted: rgb(0x66, 0x73, 0x83),
    text_faint: rgb(0x98, 0xA1, 0xAD),
    accent: rgb(0xE8, 0x93, 0x0C),
    primary: rgb(0x2F, 0x86, 0xD6),
    primary_hover: rgb(0x4D, 0x9A, 0xE0),
    primary_active: rgb(0x25, 0x71, 0xB8),
    on_primary: rgb(0xFF, 0xFF, 0xFF),
    success: rgb(0x1F, 0x9D, 0x61),
    error: rgb(0xD6, 0x45, 0x45),
    info: rgb(0x2F, 0x86, 0xD6),
};

/// Dark roast: deep espresso browns under cream text, caramel accents.
const ESPRESSO: Palette = Palette {
    name: "Espresso",
    dark: true,
    bg: rgb(0x22, 0x1A, 0x15),
    card: rgb(0x2C, 0x23, 0x1D),
    card_stroke: rgb(0x3E, 0x33, 0x2B),
    field: rgb(0x1A, 0x14, 0x0F),
    widget: rgb(0x3A, 0x2F, 0x27),
    widget_hover: rgb(0x46, 0x39, 0x2F),
    widget_active: rgb(0x52, 0x44, 0x38),
    widget_stroke: rgb(0x46, 0x3A, 0x30),
    text: rgb(0xEA, 0xDF, 0xD3),
    text_strong: rgb(0xF7, 0xEF, 0xE5),
    text_muted: rgb(0xA6, 0x96, 0x8A),
    text_faint: rgb(0x73, 0x65, 0x5A),
    accent: rgb(0xE0, 0xA2, 0x64),
    primary: rgb(0xDC, 0xAE, 0x7C),
    primary_hover: rgb(0xE8, 0xBF, 0x92),
    primary_active: rgb(0xC7, 0x9A, 0x66),
    on_primary: rgb(0x2A, 0x1C, 0x10),
    success: rgb(0x8F, 0xBF, 0x7F),
    error: rgb(0xE0, 0x7A, 0x6B),
    info: rgb(0x8F, 0xB8, 0xAB),
};

/// The same coffee, with milk: warm cream surfaces and dark-brown text.
const CAFFE_LATTE: Palette = Palette {
    name: "Caffe Latte (light)",
    dark: false,
    bg: rgb(0xF3, 0xEB, 0xDF),
    card: rgb(0xFD, 0xF8, 0xF0),
    card_stroke: rgb(0xE3, 0xD5, 0xC2),
    field: rgb(0xED, 0xE2, 0xD2),
    widget: rgb(0xE7, 0xD9, 0xC6),
    widget_hover: rgb(0xDC, 0xCB, 0xB4),
    widget_active: rgb(0xD0, 0xBC, 0xA1),
    widget_stroke: rgb(0xD9, 0xC8, 0xB2),
    text: rgb(0x45, 0x35, 0x29),
    text_strong: rgb(0x2B, 0x1F, 0x16),
    text_muted: rgb(0x8A, 0x76, 0x63),
    text_faint: rgb(0xB3, 0xA2, 0x8F),
    accent: rgb(0xB5, 0x72, 0x2E),
    primary: rgb(0x8A, 0x5A, 0x34),
    primary_hover: rgb(0x9C, 0x6C, 0x44),
    primary_active: rgb(0x74, 0x4A, 0x28),
    on_primary: rgb(0xFF, 0xF6, 0xEA),
    success: rgb(0x52, 0x88, 0x5A),
    error: rgb(0xC2, 0x54, 0x45),
    info: rgb(0x5B, 0x85, 0xAD),
};

/// Muted mauve twilight with gold and rose, after Rosé Pine.
const ROSE_PINE: Palette = Palette {
    name: "Rose Pine",
    dark: true,
    bg: rgb(0x19, 0x17, 0x24),
    card: rgb(0x1F, 0x1D, 0x2E),
    card_stroke: rgb(0x2E, 0x2B, 0x44),
    field: rgb(0x13, 0x11, 0x1E),
    widget: rgb(0x2A, 0x27, 0x3F),
    widget_hover: rgb(0x35, 0x31, 0x4F),
    widget_active: rgb(0x40, 0x3C, 0x5E),
    widget_stroke: rgb(0x38, 0x34, 0x55),
    text: rgb(0xE0, 0xDE, 0xF4),
    text_strong: rgb(0xF2, 0xF0, 0xFC),
    text_muted: rgb(0x90, 0x8C, 0xAA),
    text_faint: rgb(0x6E, 0x6A, 0x86),
    accent: rgb(0xF6, 0xC1, 0x77),
    primary: rgb(0xEB, 0xBC, 0xBA),
    primary_hover: rgb(0xF2, 0xCD, 0xCB),
    primary_active: rgb(0xD9, 0xA5, 0xA2),
    on_primary: rgb(0x33, 0x16, 0x1B),
    success: rgb(0xA8, 0xC7, 0x9A),
    error: rgb(0xEB, 0x6F, 0x92),
    info: rgb(0x9C, 0xCF, 0xD8),
};

/// Retro terminal warmth: olive-brown grays, orange and acid green.
const GRUVBOX: Palette = Palette {
    name: "Gruvbox",
    dark: true,
    bg: rgb(0x1D, 0x20, 0x21),
    card: rgb(0x28, 0x28, 0x28),
    card_stroke: rgb(0x3C, 0x38, 0x36),
    field: rgb(0x17, 0x1A, 0x1C),
    widget: rgb(0x3C, 0x38, 0x36),
    widget_hover: rgb(0x50, 0x49, 0x45),
    widget_active: rgb(0x66, 0x5C, 0x54),
    widget_stroke: rgb(0x50, 0x49, 0x45),
    text: rgb(0xEB, 0xDB, 0xB2),
    text_strong: rgb(0xFB, 0xF1, 0xC7),
    text_muted: rgb(0xA8, 0x99, 0x84),
    text_faint: rgb(0x7C, 0x6F, 0x64),
    accent: rgb(0xFE, 0x80, 0x19),
    primary: rgb(0x8E, 0xC0, 0x7C),
    primary_hover: rgb(0xA5, 0xD1, 0x94),
    primary_active: rgb(0x76, 0xA9, 0x67),
    on_primary: rgb(0x1A, 0x24, 0x18),
    success: rgb(0xB8, 0xBB, 0x26),
    error: rgb(0xFB, 0x49, 0x34),
    info: rgb(0x83, 0xA5, 0x98),
};

/// Deep teal-slate sea, after Solarized dark.
const SOLARIZED: Palette = Palette {
    name: "Solarized",
    dark: true,
    bg: rgb(0x00, 0x2B, 0x36),
    card: rgb(0x07, 0x36, 0x42),
    card_stroke: rgb(0x14, 0x49, 0x5C),
    field: rgb(0x00, 0x21, 0x2A),
    widget: rgb(0x0E, 0x42, 0x50),
    widget_hover: rgb(0x14, 0x56, 0x68),
    widget_active: rgb(0x1A, 0x64, 0x78),
    widget_stroke: rgb(0x15, 0x53, 0x65),
    text: rgb(0xA8, 0xBC, 0xBC),
    text_strong: rgb(0xEE, 0xE8, 0xD5),
    text_muted: rgb(0x7A, 0x94, 0x99),
    text_faint: rgb(0x58, 0x6E, 0x75),
    accent: rgb(0xB5, 0x89, 0x00),
    primary: rgb(0x2A, 0xA1, 0x98),
    primary_hover: rgb(0x35, 0xB5, 0xAB),
    primary_active: rgb(0x21, 0x8F, 0x87),
    on_primary: rgb(0x00, 0x20, 0x1E),
    success: rgb(0x85, 0x99, 0x00),
    error: rgb(0xDC, 0x32, 0x2F),
    info: rgb(0x26, 0x8B, 0xD2),
};

/// True black with electric cyan and mint — for OLED fans.
const CARBON: Palette = Palette {
    name: "Carbon (black)",
    dark: true,
    bg: rgb(0x00, 0x00, 0x00),
    card: rgb(0x10, 0x10, 0x14),
    card_stroke: rgb(0x23, 0x23, 0x2A),
    field: rgb(0x08, 0x08, 0x0A),
    widget: rgb(0x1B, 0x1B, 0x22),
    widget_hover: rgb(0x26, 0x26, 0x2F),
    widget_active: rgb(0x31, 0x31, 0x3C),
    widget_stroke: rgb(0x2A, 0x2A, 0x33),
    text: rgb(0xE8, 0xE8, 0xEC),
    text_strong: rgb(0xFF, 0xFF, 0xFF),
    text_muted: rgb(0x9A, 0x9A, 0xA6),
    text_faint: rgb(0x5C, 0x5C, 0x66),
    accent: rgb(0x00, 0xE0, 0xB8),
    primary: rgb(0x00, 0xC2, 0xFF),
    primary_hover: rgb(0x33, 0xCF, 0xFF),
    primary_active: rgb(0x00, 0xA5, 0xDB),
    on_primary: rgb(0x00, 0x10, 0x18),
    success: rgb(0x2E, 0xE6, 0xA6),
    error: rgb(0xFF, 0x5C, 0x7A),
    info: rgb(0x5C, 0xA8, 0xFF),
};

/// Every scheme the Theme dropdown offers, in menu order. The first is the
/// default.
pub const SCHEMES: &[Palette] = &[
    MIDNIGHT_AMBER,
    TOKYO_NIGHT,
    NORD,
    DRACULA,
    EVERFOREST,
    PAPER,
    ESPRESSO,
    CAFFE_LATTE,
    ROSE_PINE,
    GRUVBOX,
    SOLARIZED,
    CARBON,
];

static CURRENT_SCHEME: AtomicUsize = AtomicUsize::new(0);

/// The colors currently in effect. Cheap; called per widget.
pub fn palette() -> &'static Palette {
    let i = CURRENT_SCHEME.load(Ordering::Relaxed);
    &SCHEMES[i.min(SCHEMES.len() - 1)]
}

/// Look a saved scheme up by its name (how the config file stores it).
pub fn scheme_index_by_name(name: &str) -> Option<usize> {
    SCHEMES.iter().position(|s| s.name == name)
}

/// Switch to scheme `index` and restyle the context. Called once at startup
/// with the saved choice, and again whenever the user picks another scheme.
pub fn set_scheme(ctx: &egui::Context, index: usize) {
    CURRENT_SCHEME.store(index.min(SCHEMES.len() - 1), Ordering::Relaxed);
    apply(ctx);
}

// ---------- Global style ----------

/// Restyle the context from the current palette.
pub fn apply(ctx: &egui::Context) {
    let p = *palette();

    // Forcing the egui theme (rather than following the OS) means the app
    // looks exactly like the chosen scheme on every machine.
    ctx.set_theme(if p.dark {
        egui::Theme::Dark
    } else {
        egui::Theme::Light
    });

    // Ask the OS for matching window chrome — on Windows the title bar
    // follows the *system* theme by default and would clash otherwise.
    ctx.send_viewport_cmd(egui::ViewportCommand::SetTheme(if p.dark {
        egui::SystemTheme::Dark
    } else {
        egui::SystemTheme::Light
    }));

    let mut style = (*ctx.style()).clone();

    style.text_styles = [
        (TextStyle::Heading, FontId::new(18.0, FontFamily::Proportional)),
        (TextStyle::Body, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Button, FontId::new(13.5, FontFamily::Proportional)),
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (TextStyle::Monospace, FontId::new(12.0, FontFamily::Monospace)),
    ]
    .into();

    // Airier spacing than the default: room around controls is most of what
    // separates "engineer UI" from something that feels designed.
    style.spacing.item_spacing = egui::vec2(10.0, 9.0);
    style.spacing.button_padding = egui::vec2(14.0, 7.0);
    style.spacing.interact_size.y = 24.0;
    style.spacing.icon_width = 16.0;
    style.spacing.icon_width_inner = 9.0;
    style.spacing.menu_margin = Margin::same(8);
    style.spacing.scroll = egui::style::ScrollStyle::thin();

    // Labels shouldn't grab a text cursor on click-drag; it makes the whole
    // UI feel like a document instead of an app.
    style.interaction.selectable_labels = false;

    // Start from the stock visuals for the palette's lightness so anything
    // we don't set below still has a sensible value.
    let shadow_alpha = if p.dark { 110 } else { 45 };
    style.visuals = if p.dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    let v = &mut style.visuals;
    v.panel_fill = p.bg;
    v.window_fill = p.card;
    v.window_stroke = Stroke::new(1.0, p.card_stroke);
    v.window_corner_radius = CornerRadius::same(18);
    v.window_shadow = Shadow {
        offset: [0, 8],
        blur: 32,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    v.popup_shadow = Shadow {
        offset: [0, 4],
        blur: 16,
        spread: 0,
        color: Color32::from_black_alpha(shadow_alpha),
    };
    v.menu_corner_radius = CornerRadius::same(14);
    v.extreme_bg_color = p.field;
    v.code_bg_color = p.field;
    v.faint_bg_color = p.widget;

    // Plain labels and separators.
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, p.card_stroke);
    v.widgets.noninteractive.bg_fill = p.card;
    v.widgets.noninteractive.corner_radius = CornerRadius::same(12);

    // Idle widgets.
    v.widgets.inactive.bg_fill = p.widget;
    v.widgets.inactive.weak_bg_fill = p.widget;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, p.widget_stroke);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text);
    v.widgets.inactive.corner_radius = CornerRadius::same(12);

    // Hovered.
    v.widgets.hovered.bg_fill = p.widget_hover;
    v.widgets.hovered.weak_bg_fill = p.widget_hover;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, p.widget_active);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, p.text_strong);
    v.widgets.hovered.corner_radius = CornerRadius::same(12);

    // Pressed / dragged. Its fg color doubles as `strong_text_color()`,
    // which is what `RichText::strong()` renders with.
    v.widgets.active.bg_fill = p.widget_active;
    v.widgets.active.weak_bg_fill = p.widget_active;
    v.widgets.active.bg_stroke = Stroke::new(1.0, p.widget_active);
    v.widgets.active.fg_stroke = Stroke::new(1.0, p.text_strong);
    v.widgets.active.corner_radius = CornerRadius::same(12);

    // Open (a combo box showing its popup).
    v.widgets.open.bg_fill = p.widget_hover;
    v.widgets.open.weak_bg_fill = p.widget_hover;
    v.widgets.open.bg_stroke = Stroke::new(1.0, p.widget_active);
    v.widgets.open.fg_stroke = Stroke::new(1.0, p.text_strong);
    v.widgets.open.corner_radius = CornerRadius::same(12);

    // Selection: text selection, focused-field outline, combo highlight.
    v.selection.bg_fill = with_alpha(p.accent, 70);
    v.selection.stroke = Stroke::new(1.0, p.accent);
    v.hyperlink_color = p.accent;
    v.warn_fg_color = p.accent;
    v.error_fg_color = p.error;
    v.text_cursor.stroke = Stroke::new(2.0, p.accent);

    ctx.set_style(style);
}

/// `color` at `alpha` (0-255) — for translucent chip/overlay fills.
pub fn with_alpha(color: Color32, alpha: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), alpha)
}

/// How round the window itself is. Only meaningful where we draw the corners
/// ourselves — see [`window_fill`].
pub const WINDOW_RADIUS: u8 = 12;

/// What the full-bleed panels (titlebar, central, actions) should be filled
/// with.
///
/// Opaque background everywhere except macOS. The window is frameless on every
/// platform, and each OS drops the rounded corners along with the title bar.
/// Windows hands them back through DWM, so its panels can stay opaque. macOS
/// has no equivalent call, so the corners have to be painted — which only
/// works if the panels covering them are transparent and something rounded is
/// drawn underneath. See `paint_window_background`.
pub fn window_fill() -> Color32 {
    if cfg!(target_os = "macos") {
        Color32::TRANSPARENT
    } else {
        palette().bg
    }
}

/// Paint the window's own background, rounded, beneath everything else.
///
/// A no-op away from macOS, where the panels are opaque and the OS rounds the
/// window for us.
pub fn paint_window_background(ctx: &egui::Context) {
    if !cfg!(target_os = "macos") {
        return;
    }
    let rect = ctx.screen_rect();
    ctx.layer_painter(egui::LayerId::background()).rect_filled(
        rect,
        CornerRadius::same(WINDOW_RADIUS),
        palette().bg,
    );
}

// ---------- Building blocks ----------

/// A raised card: the panel sections and each queue row live in one.
pub fn card() -> egui::Frame {
    let p = palette();
    egui::Frame::new()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.card_stroke))
        .corner_radius(CornerRadius::same(18))
        .inner_margin(Margin::symmetric(16, 14))
}

/// Small uppercase section header inside a card.
pub fn section_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).small().color(palette().text_faint));
}

/// A rounded status pill: tinted background, colored text.
pub fn chip(ui: &mut egui::Ui, text: &str, color: Color32) -> egui::Response {
    egui::Frame::new()
        .fill(with_alpha(color, 26))
        .corner_radius(CornerRadius::same(255))
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).small().color(color));
        })
        .response
}

/// The filled call-to-action button. Falls back to a plain (properly
/// dimmed) button when disabled, so the color never shows half-greyed.
pub fn primary_button(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    if !enabled {
        return ui.add_enabled(false, egui::Button::new(text));
    }
    let p = *palette();
    ui.scope(|ui| {
        let v = &mut ui.style_mut().visuals;
        for w in [&mut v.widgets.inactive, &mut v.widgets.hovered, &mut v.widgets.active] {
            w.fg_stroke = Stroke::new(1.0, p.on_primary);
            w.bg_stroke = Stroke::NONE;
        }
        v.widgets.inactive.weak_bg_fill = p.primary;
        v.widgets.hovered.weak_bg_fill = p.primary_hover;
        v.widgets.active.weak_bg_fill = p.primary_active;
        ui.add(egui::Button::new(egui::RichText::new(text).color(p.on_primary)))
    })
    .inner
}

/// A quiet, borderless button — for secondary actions that shouldn't compete
/// visually (remove ✖, retry, "Show in folder"). Lights up on hover.
pub fn ghost_button(ui: &mut egui::Ui, text: &str) -> egui::Response {
    let p = *palette();
    ui.scope(|ui| {
        let v = &mut ui.style_mut().visuals;
        v.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
        v.widgets.inactive.bg_stroke = Stroke::NONE;
        v.widgets.inactive.fg_stroke = Stroke::new(1.0, p.text_muted);
        v.widgets.hovered.bg_stroke = Stroke::NONE;
        v.widgets.active.bg_stroke = Stroke::NONE;
        ui.add(egui::Button::new(text))
    })
    .inner
}

/// An on/off switch: a pill track with a knob that slides across, then its
/// label.
///
/// Same contract as `ui.checkbox` — clicking either the switch or its label
/// flips `on`, and the returned `Response` reports `changed()` — but it reads
/// as a setting you leave switched on or off, rather than an item you tick.
/// Used for the app-wide toggles, which is what tells them apart from the
/// per-clip checkboxes.
pub fn toggle_switch(ui: &mut egui::Ui, on: &mut bool, label: &str) -> egui::Response {
    let p = *palette();

    ui.horizontal(|ui| {
        let (rect, switch) =
            ui.allocate_exact_size(egui::vec2(28.0, 16.0), egui::Sense::click());
        // Keep the switch's own id for the animation: the union below may
        // report the label's, and the knob has to animate against a stable one.
        let id = switch.id;

        // The label is part of the control, the way a checkbox's is.
        let text = ui.add(
            egui::Label::new(egui::RichText::new(label).color(p.text))
                .sense(egui::Sense::click()),
        );

        let mut response = switch.union(text);
        if response.clicked() {
            *on = !*on;
            response.mark_changed();
        }

        if ui.is_rect_visible(rect) {
            // Slide the knob rather than snapping it, so the state change is
            // legible even out of the corner of your eye.
            let how_on = ui.ctx().animate_bool(id, *on);
            let radius = 0.5 * rect.height();
            let track = match (*on, response.hovered()) {
                (true, true) => p.primary_hover,
                (true, false) => p.primary,
                (false, true) => p.widget_hover,
                (false, false) => p.widget,
            };
            ui.painter()
                .rect_filled(rect, CornerRadius::same(radius as u8), track);
            let knob_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), how_on);
            let knob = if *on { p.on_primary } else { p.text_muted };
            ui.painter()
                .circle_filled(egui::pos2(knob_x, rect.center().y), radius - 2.5, knob);
        }

        response
    })
    .inner
}

/// Slim accent progress bar with its label alongside rather than on the bar
/// (light text on the accent color is unreadable; beside it, both stay crisp).
pub fn progress_bar(ui: &mut egui::Ui, frac: f32, label: &str) {
    let p = *palette();
    ui.horizontal(|ui| {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(label).small().color(p.text_muted));
            ui.add(
                egui::ProgressBar::new(frac)
                    .fill(p.accent)
                    .desired_height(8.0)
                    .desired_width(ui.available_width()),
            );
        });
    });
}
