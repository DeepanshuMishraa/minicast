//! The window's look: near-black ink surfaces, hairline borders, square
//! corners, and one orange accent. Components read these tokens, so nothing
//! else in the GUI names a color literal.
use gpui_kit::component::{Theme, ThemeConfig};
use gpui_kit::App;
use std::rc::Rc;

const PALETTE: &str = r##"{
  "name": "minicast",
  "mode": "dark",
  "radius": 0,
  "radius.lg": 0,
  "colors": {
    "background": "#09090b",
    "foreground": "#ececf1",
    "border": "#202028",
    "ring": "#ff7a1a",
    "selection.background": "#ff7a1a55",
    "sidebar.background": "#0d0d10",
    "sidebar.foreground": "#ececf1",
    "sidebar.border": "#202028",
    "group_box.background": "#121216",
    "popover.background": "#121216",
    "popover.foreground": "#ececf1",
    "muted.background": "#17171c",
    "muted.foreground": "#7b7b88",
    "secondary.background": "#17171c",
    "secondary.hover.background": "#1e1e25",
    "secondary.active.background": "#26262e",
    "secondary.foreground": "#ececf1",
    "accent.background": "#1e1e25",
    "accent.foreground": "#ececf1",
    "primary.background": "#ff7a1a",
    "primary.hover.background": "#ff8f3d",
    "primary.active.background": "#e8680c",
    "primary.foreground": "#190c02",
    "danger.background": "#ff5d6c",
    "danger.hover.background": "#ff7783",
    "danger.active.background": "#e84a59",
    "danger.foreground": "#ffffff",
    "success.background": "#3ddc97",
    "success.foreground": "#06140d",
    "warning.background": "#ffb454",
    "warning.foreground": "#1a1105",
    "input.border": "#2a2a33",
    "slider.background": "#ff7a1a",
    "slider.thumb.background": "#ececf1",
    "switch.background": "#2a2a33",
    "switch.thumb.background": "#ececf1",
    "scrollbar.thumb.background": "#2a2a33",
    "scrollbar.thumb.hover.background": "#3a3a46"
  }
}"##;

pub fn apply(cx: &mut App) {
    match serde_json::from_str::<ThemeConfig>(PALETTE) {
        Ok(config) => {
            let config = Rc::new(config);
            Theme::update(cx, |theme| theme.apply_config(&config));
        }
        // The palette is a compile-time constant, so this only fires if a
        // gpui-component upgrade renames a token. Fall back to flat corners.
        Err(e) => {
            eprintln!("minicast gui: built-in palette rejected ({e}); using the default dark theme");
            Theme::update(cx, |theme| {
                theme.radius = gpui_kit::px(0.0);
                theme.radius_lg = gpui_kit::px(0.0);
            });
        }
    }
}
