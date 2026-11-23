//! OS-aware custom window titlebar merged with the top toolbar.
//!
//! Adapts height, padding, surface styling, drag/double-click behaviour, and
//! window control button placement/appearance to macOS, Windows, and Linux:
//!
//! - **macOS**: native traffic lights on the left (`appears_transparent` +
//!   `traffic_light_position`), left inset reserved while windowed (collapsed
//!   in fullscreen), macOS unified-toolbar gradient, and `titlebar_double_click`.
//! - **Windows**: Fluent-style 46 px flush rectangular caption buttons on the
//!   right with native `WindowControlArea` hit-testing (preserving Windows 11
//!   Snap Layouts) and a red close hover state.
//! - **Linux**: client-side decorations (`WindowDecorations::Client`) with
//!   circular CSD buttons on the right, rounded top window corners matching
//!   `window_corner_radii`, right-click system window menu, and graceful
//!   fallback (no duplicate controls) when the WM enforces server decorations.

use gpui_kit::component::{ActiveTheme, Icon, IconName, InteractiveElementExt as _, h_flex, window_corner_radii};
use gpui_kit::prelude::*;
use gpui_kit::*;

const MACOS_TITLEBAR_HEIGHT: Pixels = px(40.);
const DEFAULT_TITLEBAR_HEIGHT: Pixels = px(42.);
const MACOS_TRAFFIC_LIGHT_INSET: Pixels = px(78.);
const WINDOWS_CONTROL_WIDTH: Pixels = px(46.);
const LINUX_CONTROL_SIZE: Pixels = px(26.);
const CONTENT_PADDING: Pixels = px(12.);

/// Runtime / compile-time desktop operating system.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlatformOs {
    MacOs,
    Windows,
    Linux,
}

impl PlatformOs {
    /// Returns the active OS. In debug builds, `TESTUDO_DEBUG_OS` (`macos`,
    /// `windows`, or `linux`) can override this to preview another OS's chrome.
    pub fn current() -> Self {
        #[cfg(debug_assertions)]
        if let Ok(override_os) = std::env::var("TESTUDO_DEBUG_OS") {
            match override_os.to_ascii_lowercase().as_str() {
                "macos" | "mac" | "darwin" => return Self::MacOs,
                "windows" | "win" => return Self::Windows,
                "linux" => return Self::Linux,
                _ => {}
            }
        }
        Self::compiled()
    }

    pub const fn compiled() -> Self {
        #[cfg(target_os = "macos")]
        {
            Self::MacOs
        }
        #[cfg(target_os = "windows")]
        {
            Self::Windows
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Self::Linux
        }
    }
}

/// Where window control buttons live in the titlebar.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlsSide {
    /// Native macOS traffic lights painted by AppKit on the left.
    NativeMacOsLeft,
    /// Custom window controls placed on the right (Windows / Linux).
    Right,
}

/// Visual style of custom-drawn window control buttons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlButtonStyle {
    /// Windows 11 Fluent caption buttons: flush 46 px wide rectangles.
    WindowsFluent,
    /// Linux CSD buttons: circular 26 px hover targets with inset padding.
    LinuxRounded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WindowControlKind {
    Minimize,
    Maximize,
    Restore,
    Close,
}

/// Pure, OS-derived layout and styling specification for the custom titlebar.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TitlebarSpec {
    pub os: PlatformOs,
    pub height: Pixels,
    pub left_padding: Pixels,
    pub right_padding: Pixels,
    pub controls_side: ControlsSide,
    pub button_style: Option<ControlButtonStyle>,
    pub show_controls_separator: bool,
    pub traffic_light_position: Option<Point<Pixels>>,
}

impl TitlebarSpec {
    pub fn for_os(os: PlatformOs, is_fullscreen: bool, is_client_decorated: bool) -> Self {
        match os {
            PlatformOs::MacOs => Self {
                os,
                height: MACOS_TITLEBAR_HEIGHT,
                left_padding: if is_fullscreen {
                    CONTENT_PADDING
                } else {
                    MACOS_TRAFFIC_LIGHT_INSET
                },
                right_padding: CONTENT_PADDING,
                controls_side: ControlsSide::NativeMacOsLeft,
                button_style: None,
                show_controls_separator: false,
                traffic_light_position: Some(point(px(12.), px(13.))),
            },
            PlatformOs::Windows => Self {
                os,
                height: DEFAULT_TITLEBAR_HEIGHT,
                left_padding: CONTENT_PADDING,
                right_padding: px(0.),
                controls_side: ControlsSide::Right,
                button_style: Some(ControlButtonStyle::WindowsFluent),
                show_controls_separator: true,
                traffic_light_position: None,
            },
            PlatformOs::Linux => {
                let show_controls = is_client_decorated;
                Self {
                    os,
                    height: DEFAULT_TITLEBAR_HEIGHT,
                    left_padding: CONTENT_PADDING,
                    right_padding: if show_controls { px(8.) } else { CONTENT_PADDING },
                    controls_side: ControlsSide::Right,
                    button_style: show_controls.then_some(ControlButtonStyle::LinuxRounded),
                    show_controls_separator: show_controls,
                    traffic_light_position: None,
                }
            }
        }
    }
}

/// Window options configured for the OS-aware custom titlebar.
pub fn window_options(cx: &App) -> WindowOptions {
    let spec = TitlebarSpec::for_os(PlatformOs::compiled(), false, true);
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(1400.), px(900.)),
            cx,
        ))),
        titlebar: Some(TitlebarOptions {
            title: Some("Testudo".into()),
            appears_transparent: true,
            traffic_light_position: spec.traffic_light_position,
        }),
        // We draw the titlebar and call `start_window_move` ourselves, so
        // AppKit should not intercept drags or delay titlebar clicks.
        app_owns_titlebar_drag: true,
        window_decorations: Some(WindowDecorations::Client),
        app_id: Some("testudo".into()),
        ..Default::default()
    }
}

struct TitlebarDragState {
    should_move: bool,
}

impl Render for TitlebarDragState {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn titlebar_background(os: PlatformOs, title_bar: Hsla, background: Hsla) -> Background {
    match os {
        PlatformOs::Windows => title_bar.into(),
        PlatformOs::MacOs => {
            let top = blend_hsla(title_bar, background, 0.35);
            linear_gradient(180., linear_color_stop(top, 0.), linear_color_stop(title_bar, 1.))
        }
        PlatformOs::Linux => {
            let top = blend_hsla(title_bar, background, 0.55);
            linear_gradient(180., linear_color_stop(top, 0.), linear_color_stop(title_bar, 1.))
        }
    }
}

fn blend_hsla(a: Hsla, b: Hsla, a_weight: f32) -> Hsla {
    let b_weight = 1.0 - a_weight;
    let a = a.to_rgb();
    let b = b.to_rgb();
    Hsla::from(Rgba {
        r: a.r * a_weight + b.r * b_weight,
        g: a.g * a_weight + b.g * b_weight,
        b: a.b * a_weight + b.b * b_weight,
        a: a.a * a_weight + b.a * b_weight,
    })
}

fn control_order(side: ControlsSide, supported: WindowControls, is_maximized: bool) -> Vec<WindowControlKind> {
    let max_kind = if is_maximized {
        WindowControlKind::Restore
    } else {
        WindowControlKind::Maximize
    };
    let mut controls = Vec::with_capacity(3);
    match side {
        ControlsSide::NativeMacOsLeft => {}
        ControlsSide::Right => {
            if supported.minimize {
                controls.push(WindowControlKind::Minimize);
            }
            if supported.maximize {
                controls.push(max_kind);
            }
            controls.push(WindowControlKind::Close);
        }
    }
    controls
}

fn drag_region(
    id: &'static str,
    os: PlatformOs,
    is_client_decorated: bool,
    drag_state: &Entity<TitlebarDragState>,
    window: &mut Window,
) -> Stateful<Div> {
    let use_native_windows_drag = cfg!(target_os = "windows");
    let use_macos_double_click = os == PlatformOs::MacOs || cfg!(target_os = "macos");
    let use_linux_chrome = os == PlatformOs::Linux || cfg!(target_os = "linux");

    div()
        .id(id)
        .h_full()
        .flex()
        .items_center()
        .window_control_area(WindowControlArea::Drag)
        .when(!use_native_windows_drag, |this| {
            this.when(use_macos_double_click, |d| {
                d.on_double_click(|_, window, _| window.titlebar_double_click())
            })
            .when(!use_macos_double_click && use_linux_chrome, |d| {
                d.on_double_click(|_, window, _| window.zoom_window())
            })
            .on_mouse_down_out(window.listener_for(drag_state, |state, _, _, _| {
                state.should_move = false;
            }))
            .on_mouse_down(
                MouseButton::Left,
                window.listener_for(drag_state, |state, _, _, _| {
                    state.should_move = true;
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                window.listener_for(drag_state, |state, _, _, _| {
                    state.should_move = false;
                }),
            )
            .on_mouse_move(window.listener_for(drag_state, |state, _, window, _| {
                if state.should_move {
                    state.should_move = false;
                    window.start_window_move();
                }
            }))
        })
        .when(use_linux_chrome && is_client_decorated, |this| {
            this.on_mouse_down(MouseButton::Right, |ev, window, _| {
                window.show_window_menu(ev.position);
            })
        })
}

fn render_window_controls(spec: TitlebarSpec, window: &Window, cx: &App) -> Option<AnyElement> {
    let style = spec.button_style?;
    let controls = control_order(spec.controls_side, window.window_controls(), window.is_maximized());
    if controls.is_empty() {
        return None;
    }

    let is_window_active = window.is_window_active();
    let is_windows_native = cfg!(target_os = "windows");
    let fg = if is_window_active {
        cx.theme().foreground
    } else {
        cx.theme().muted_foreground
    };

    let mut row = h_flex().id("window-controls").items_center().flex_shrink_0();
    row = match style {
        ControlButtonStyle::WindowsFluent => row.h_full(),
        ControlButtonStyle::LinuxRounded => row.h_full().gap_1(),
    };

    for kind in controls {
        let is_close = kind == WindowControlKind::Close;
        let id = match kind {
            WindowControlKind::Minimize => "window-minimize",
            WindowControlKind::Maximize => "window-maximize",
            WindowControlKind::Restore => "window-restore",
            WindowControlKind::Close => "window-close",
        };
        let icon = match kind {
            WindowControlKind::Minimize => IconName::WindowMinimize,
            WindowControlKind::Maximize => IconName::WindowMaximize,
            WindowControlKind::Restore => IconName::WindowRestore,
            WindowControlKind::Close => IconName::WindowClose,
        };
        let area = match kind {
            WindowControlKind::Minimize => WindowControlArea::Min,
            WindowControlKind::Maximize | WindowControlKind::Restore => WindowControlArea::Max,
            WindowControlKind::Close => WindowControlArea::Close,
        };

        let (hover_bg, active_bg, hover_fg) = if is_close {
            match style {
                ControlButtonStyle::WindowsFluent => {
                    (rgb(0xc42b1c).into(), rgb(0xb32619).into(), cx.theme().danger_foreground)
                }
                ControlButtonStyle::LinuxRounded => (
                    cx.theme().danger,
                    cx.theme().danger_active,
                    cx.theme().danger_foreground,
                ),
            }
        } else {
            (
                cx.theme().secondary_hover,
                cx.theme().secondary_active,
                cx.theme().secondary_foreground,
            )
        };

        let mut btn = div()
            .id(id)
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .text_color(fg)
            .hover(move |s| s.bg(hover_bg).text_color(hover_fg))
            .active(move |s| s.bg(active_bg).text_color(hover_fg));

        btn = match style {
            ControlButtonStyle::WindowsFluent => btn.w(WINDOWS_CONTROL_WIDTH).h_full(),
            ControlButtonStyle::LinuxRounded => btn.size(LINUX_CONTROL_SIZE).rounded_full(),
        };

        btn = if is_windows_native {
            btn.window_control_area(area)
        } else {
            btn.on_mouse_down(MouseButton::Left, |_, window, cx| {
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_click(move |_, window, cx| {
                cx.stop_propagation();
                match kind {
                    WindowControlKind::Minimize => window.minimize_window(),
                    WindowControlKind::Maximize | WindowControlKind::Restore => window.zoom_window(),
                    WindowControlKind::Close => window.remove_window(),
                }
            })
        };

        row = row.child(btn.child(Icon::new(icon).size(px(12.))));
    }

    Some(row.into_any_element())
}

/// Renders the OS-aware custom titlebar with merged toolbar content.
///
/// `brand` and the flexible spacer between `toolbar` and `trailing` act as
/// window drag / double-click regions, while `toolbar` and `trailing` remain
/// normal interactive areas.
pub fn render_titlebar(
    window: &mut Window,
    cx: &mut App,
    brand: impl IntoElement,
    toolbar: impl IntoElement,
    trailing: impl IntoElement,
) -> impl IntoElement {
    let os = PlatformOs::current();
    let is_client_decorated = matches!(window.window_decorations(), Decorations::Client { .. });
    let spec = TitlebarSpec::for_os(os, window.is_fullscreen(), is_client_decorated);
    let drag_state = window.use_state(cx, |_, _| TitlebarDragState { should_move: false });
    let radii = window_corner_radii(window, cx);
    let is_window_active = window.is_window_active();

    let right_controls = match spec.controls_side {
        ControlsSide::Right => render_window_controls(spec, window, cx),
        ControlsSide::NativeMacOsLeft => None,
    };

    let brand_drag = drag_region("titlebar-brand-drag", os, is_client_decorated, &drag_state, window)
        .flex_none()
        .pr_1()
        .when(!is_window_active, |d| d.opacity(0.75))
        .child(brand);

    let spacer_drag = drag_region("titlebar-spacer-drag", os, is_client_decorated, &drag_state, window)
        .flex_1()
        .min_w(px(24.));

    h_flex()
        .id("titlebar")
        .flex_none()
        .w_full()
        .h(spec.height)
        .pl(spec.left_padding)
        .pr(spec.right_padding)
        .gap_2()
        .items_center()
        .rounded_tl(radii.top_left)
        .rounded_tr(radii.top_right)
        .border_b_1()
        .border_color(cx.theme().title_bar_border)
        .bg(titlebar_background(
            spec.os,
            cx.theme().title_bar,
            cx.theme().background,
        ))
        .child(brand_drag)
        .child(toolbar)
        .child(spacer_drag)
        .child(
            h_flex()
                .flex_none()
                .h_full()
                .items_center()
                .gap_1()
                .child(trailing)
                .when(spec.show_controls_separator && right_controls.is_some(), |row| {
                    row.child(div().w(px(1.)).h(px(16.)).mx_1().bg(cx.theme().border))
                })
                .children(right_controls),
        )
}

#[cfg(test)]
mod tests {
    use gpui_kit::{WindowControls, px};

    use super::{
        CONTENT_PADDING, ControlButtonStyle, ControlsSide, MACOS_TRAFFIC_LIGHT_INSET, PlatformOs, TitlebarSpec,
        WindowControlKind, control_order,
    };

    #[test]
    fn macos_spec_reserves_traffic_lights_when_windowed() {
        let windowed = TitlebarSpec::for_os(PlatformOs::MacOs, false, true);
        assert_eq!(windowed.controls_side, ControlsSide::NativeMacOsLeft);
        assert_eq!(windowed.button_style, None);
        assert_eq!(windowed.left_padding, MACOS_TRAFFIC_LIGHT_INSET);
        assert!(windowed.traffic_light_position.is_some());
        assert!(!windowed.show_controls_separator);

        let fullscreen = TitlebarSpec::for_os(PlatformOs::MacOs, true, true);
        assert_eq!(fullscreen.left_padding, CONTENT_PADDING);
    }

    #[test]
    fn windows_spec_uses_flush_fluent_controls_on_right() {
        let spec = TitlebarSpec::for_os(PlatformOs::Windows, false, true);
        assert_eq!(spec.controls_side, ControlsSide::Right);
        assert_eq!(spec.button_style, Some(ControlButtonStyle::WindowsFluent));
        assert_eq!(spec.right_padding, px(0.));
        assert!(spec.traffic_light_position.is_none());
        assert!(spec.show_controls_separator);
    }

    #[test]
    fn linux_spec_skips_custom_controls_under_server_decorations() {
        let client = TitlebarSpec::for_os(PlatformOs::Linux, false, true);
        assert_eq!(client.controls_side, ControlsSide::Right);
        assert_eq!(client.button_style, Some(ControlButtonStyle::LinuxRounded));
        assert!(client.show_controls_separator);

        let server = TitlebarSpec::for_os(PlatformOs::Linux, false, false);
        assert_eq!(server.button_style, None);
        assert_eq!(server.right_padding, CONTENT_PADDING);
        assert!(!server.show_controls_separator);
    }

    #[test]
    fn control_order_respects_side_and_wm_capabilities() {
        let all = WindowControls {
            minimize: true,
            maximize: true,
            window_menu: true,
            fullscreen: false,
        };
        assert_eq!(
            control_order(ControlsSide::Right, all, false),
            vec![
                WindowControlKind::Minimize,
                WindowControlKind::Maximize,
                WindowControlKind::Close,
            ]
        );
        assert_eq!(
            control_order(ControlsSide::Right, all, true),
            vec![
                WindowControlKind::Minimize,
                WindowControlKind::Restore,
                WindowControlKind::Close,
            ]
        );

        let tiling_wm = WindowControls {
            minimize: false,
            maximize: false,
            window_menu: false,
            fullscreen: false,
        };
        assert_eq!(
            control_order(ControlsSide::Right, tiling_wm, false),
            vec![WindowControlKind::Close]
        );
        assert!(control_order(ControlsSide::NativeMacOsLeft, all, false).is_empty());
    }
}
