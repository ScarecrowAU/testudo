//! The settings dialog: recording resolution and frame rate, where recordings
//! are kept, and deleting old or all recordings.
//!
//! Its own view (not state captured in the dialog builder, which gpui-kit may
//! rebuild at any time); it reads and changes the app through a weak handle.

use std::rc::Rc;

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::ButtonVariant;
use testudo_core::settings::{DEFAULT_FRAME_RATE, FRAME_RATES};
use testudo_store::fallback_data_dir;

use super::*;

/// "Delete recordings older than" choices, in days.
const AGE_CHOICES: [u32; 6] = [1, 3, 7, 14, 30, 60];
const DEFAULT_AGE: u32 = 30;
const DAY_MS: f64 = 86_400_000.0;

pub struct SettingsPanel {
    app: WeakEntity<TestudoApp>,
    /// The app's history store (opened at startup), for usage and deleting.
    store: Option<Rc<Store>>,
    older_than_days: u32,
    /// (finished runs, bytes on disk), measured when opened and after deleting.
    usage: Option<(u64, u64)>,
    status: Option<String>,
}

impl SettingsPanel {
    /// Built while the app is being updated (its click handler), so it must
    /// not read the app here: the store is passed in.
    pub fn new(app: WeakEntity<TestudoApp>, store: Option<Rc<Store>>) -> Self {
        let mut panel = Self {
            app,
            store,
            older_than_days: DEFAULT_AGE,
            usage: None,
            status: None,
        };
        panel.measure();
        panel
    }

    fn measure(&mut self) {
        self.usage = self.store.as_ref().and_then(|s| s.usage().ok());
    }

    /// Change a setting in the app, save it, and apply it to new runs.
    fn update_settings(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Settings)) {
        let result = self.app.update(cx, |app, cx| {
            change(&mut app.settings);
            app.orchestrator.set_recording(&app.settings);
            cx.notify();
            app.settings.save()
        });
        self.status = match result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(format!("Could not save settings: {e:#}")),
            Err(_) => None,
        };
        cx.notify();
    }

    fn pick_location(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Choose where to keep recordings".into()),
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await
                && let Some(dir) = paths.into_iter().next()
            {
                let _ = this.update(cx, |p, cx| p.update_settings(cx, |s| s.recordings_dir = Some(dir)));
            }
        })
        .detach();
    }

    /// Delete finished runs started before `before_ms` (or all of them).
    fn delete(&mut self, before_ms: Option<f64>, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else { return };
        self.status = Some(match store.delete_runs(before_ms) {
            Ok(r) if r.runs_deleted == 0 => "Nothing to delete.".to_owned(),
            Ok(r) => format!(
                "Deleted {} run{}, freed {}.",
                r.runs_deleted,
                if r.runs_deleted == 1 { "" } else { "s" },
                fmt_bytes(r.frame_bytes)
            ),
            Err(e) => format!("Delete failed: {e:#}"),
        });
        let _ = self.app.update(cx, |app, cx| {
            app.after_recordings_deleted();
            cx.notify();
        });
        self.measure();
        cx.notify();
    }

    /// Ask before deleting; nothing is undoable here.
    fn confirm_delete(&mut self, before_ms: Option<f64>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(store) = self.store.clone() else { return };
        let count = store.count_runs(before_ms).unwrap_or(0);
        if count == 0 {
            self.status = Some("Nothing to delete.".to_owned());
            cx.notify();
            return;
        }
        let what = match before_ms {
            Some(_) => format!("Delete recordings older than {}?", days_label(self.older_than_days)),
            None => "Delete all recordings?".to_owned(),
        };
        let detail = format!(
            "{count} finished run{} and their history will be removed. Runs in progress are kept. This can't be undone.",
            if count == 1 { "" } else { "s" }
        );
        let panel = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let panel = panel.clone();
            alert
                .confirm()
                .title(what.clone())
                .description(detail.clone())
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    let _ = panel.update(cx, |p, cx| p.delete(before_ms, cx));
                    true
                })
        });
    }
}

impl Render for SettingsPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(app) = self.app.upgrade() else { return v_flex() };
        let app = app.read(cx);
        let settings = app.settings.clone();
        let current_dir = self.store.as_ref().map(|s| s.dir().to_path_buf());
        let chosen_dir = settings.recordings_dir.clone().unwrap_or_else(fallback_data_dir);
        let env_override = std::env::var_os("TESTUDO_DATA_DIR").is_some();
        // With the override, show the folder actually in use.
        let shown_dir = if env_override {
            current_dir.clone().unwrap_or_else(|| chosen_dir.clone())
        } else {
            chosen_dir.clone()
        };
        let pending_move = !env_override && current_dir.as_ref().is_some_and(|d| *d != chosen_dir);

        let this = cx.entity().downgrade();
        let resolution = choice_menu(
            "set-resolution",
            format!("{0} × {0} px", settings.frame_max()),
            FRAME_SIZES.iter().map(|&px| {
                let label = if px == DEFAULT_FRAME_MAX {
                    format!("{px} × {px} px (default)")
                } else {
                    format!("{px} × {px} px")
                };
                (label, px == settings.frame_max(), px)
            }),
            {
                let this = this.clone();
                move |px, cx| {
                    let _ = this.update(cx, |p, cx| p.update_settings(cx, |s| s.frame_max_px = px));
                }
            },
        );
        let rate = choice_menu(
            "set-frame-rate",
            rate_label(settings.frame_rate()),
            FRAME_RATES
                .iter()
                .map(|&fps| (rate_label(fps), fps == settings.frame_rate(), fps)),
            {
                let this = this.clone();
                move |fps, cx| {
                    let _ = this.update(cx, |p, cx| p.update_settings(cx, |s| s.frame_rate = fps));
                }
            },
        );
        let age = choice_menu(
            "older-than",
            days_label(self.older_than_days),
            AGE_CHOICES
                .iter()
                .map(|&d| (days_label(d), d == self.older_than_days, d)),
            {
                let this = this.clone();
                move |d, cx| {
                    let _ = this.update(cx, |p, cx| {
                        p.older_than_days = d;
                        cx.notify();
                    });
                }
            },
        );

        let section = |title: &'static str| {
            div()
                .pt_2()
                .text_xs()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(muted)
                .child(title)
        };
        let row = |label: &'static str, hint: Option<&'static str>, control: AnyElement| {
            h_flex()
                .gap_4()
                .items_center()
                .justify_between()
                .child(
                    v_flex()
                        .min_w_0()
                        .child(div().text_sm().child(label))
                        .children(hint.map(|h| div().text_xs().text_color(muted).child(h))),
                )
                .child(div().flex_none().child(control))
        };
        let usage = self.usage.map_or("measuring…".to_owned(), |(runs, bytes)| {
            format!("{runs} run{} · {}", if runs == 1 { "" } else { "s" }, fmt_bytes(bytes))
        });
        let days = self.older_than_days;

        v_flex()
            .gap_3()
            .child(section("RECORDING"))
            .child(row(
                "Resolution",
                Some("Frames fit within this box, keeping the page's shape."),
                resolution.into_any_element(),
            ))
            .child(row(
                "Frame rate",
                Some("Lower rates save space; a page's last frame is always kept."),
                rate.into_any_element(),
            ))
            .child(
                div()
                    .text_xs()
                    .text_color(muted)
                    .child("Applies to runs started from now on."),
            )
            .child(div().h(px(1.)).bg(theme.border))
            .child(section("STORAGE"))
            .child(
                v_flex()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().text_sm().flex_none().child("Location"))
                            .child(
                                div()
                                    .id("rec-dir")
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_xs()
                                    .font_family(crate::fonts::mono())
                                    .text_color(muted)
                                    .child(tilde(&shown_dir))
                                    .tooltip(tooltip_text(shown_dir.display().to_string())),
                            )
                            .child(
                                button("rec-dir-change")
                                    .label("Change…")
                                    .small()
                                    .outline()
                                    .on_click(cx.listener(|p, _, _, cx| p.pick_location(cx))),
                            )
                            .when(settings.recordings_dir.is_some(), |r| {
                                r.child(button("rec-dir-default").label("Default").small().ghost().on_click(
                                    cx.listener(|p, _, _, cx| p.update_settings(cx, |s| s.recordings_dir = None)),
                                ))
                            }),
                    )
                    .when(env_override, |d| {
                        d.child(
                            div()
                                .text_xs()
                                .text_color(theme.yellow)
                                .child("TESTUDO_DATA_DIR is set and overrides this."),
                        )
                    })
                    .when(pending_move, |d| {
                        d.child(div().text_xs().text_color(theme.yellow).child(
                            "Takes effect the next time Testudo starts. Existing recordings stay where they are.",
                        ))
                    })
                    .child(div().text_xs().text_color(muted).child(format!("Using {usage}"))),
            )
            .child(row(
                "Delete recordings older than",
                None,
                h_flex()
                    .gap_2()
                    .child(age)
                    .child(
                        button("delete-older")
                            .label("Delete")
                            .small()
                            .outline()
                            .on_click(cx.listener(move |p, _, window, cx| {
                                p.confirm_delete(Some(now_ms() - days as f64 * DAY_MS), window, cx)
                            })),
                    )
                    .into_any_element(),
            ))
            .child(row(
                "Delete all recordings",
                Some("Runs in progress are kept."),
                button("delete-all")
                    .label("Delete all")
                    .small()
                    .danger()
                    .on_click(cx.listener(|p, _, window, cx| p.confirm_delete(None, window, cx)))
                    .into_any_element(),
            ))
            .children(
                self.status
                    .clone()
                    .map(|s| div().text_sm().text_color(theme.green).child(s)),
            )
    }
}

/// An outlined button showing the current choice, opening a menu of options.
fn choice_menu<T: Copy + 'static>(
    id: &'static str,
    current: String,
    options: impl Iterator<Item = (String, bool, T)>,
    on_pick: impl Fn(T, &mut App) + 'static,
) -> impl IntoElement {
    let options: Rc<Vec<(String, bool, T)>> = Rc::new(options.collect());
    let on_pick = Rc::new(on_pick);
    button(id)
        .label(current)
        .small()
        .outline()
        .dropdown_caret(true)
        .dropdown_menu_with_anchor(Anchor::TopRight, move |mut menu, _, _| {
            for (label, checked, value) in options.iter().cloned() {
                let on_pick = on_pick.clone();
                menu = menu.item(
                    PopupMenuItem::new(label)
                        .checked(checked)
                        .on_click(move |_, _, cx| on_pick(value, cx)),
                );
            }
            menu
        })
}

fn rate_label(fps: u32) -> String {
    if fps >= DEFAULT_FRAME_RATE {
        "Every repaint (up to 60 fps)".to_owned()
    } else {
        format!("{fps} fps")
    }
}

fn days_label(d: u32) -> String {
    if d == 1 {
        "1 day".to_owned()
    } else {
        format!("{d} days")
    }
}

fn fmt_bytes(b: u64) -> String {
    match b {
        0..1_000_000 => format!("{:.0} KB", b as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.0} MB", b as f64 / 1e6),
        _ => format!("{:.1} GB", b as f64 / 1e9),
    }
}
