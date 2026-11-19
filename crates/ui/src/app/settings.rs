//! The settings dialog: recording resolution and frame rate, where recordings
//! are kept, deleting old or all recordings, and updates.
//!
//! Its own view (not state captured in the dialog builder, which gpui-kit may
//! rebuild at any time); it reads and changes the app through a weak handle.

use std::rc::Rc;

use gpui_kit::component::WindowExt;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem, SettingPage, Settings as SettingsView};
use testudo_core::settings::{DEFAULT_FRAME_RATE, FRAME_RATES};

use super::update::UpdateState;
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
    /// Re-render when the app changes (an update check finishing).
    _observe: Option<Subscription>,
}

impl SettingsPanel {
    /// Built while the app is being updated (its click handler), so it must
    /// not read the app here: the store is passed in.
    pub fn new(app: WeakEntity<TestudoApp>, store: Option<Rc<Store>>, cx: &mut Context<Self>) -> Self {
        let _observe = app.upgrade().map(|app| cx.observe(&app, |_, _, cx| cx.notify()));
        let mut panel = Self {
            app,
            store,
            older_than_days: DEFAULT_AGE,
            usage: None,
            status: None,
            _observe,
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
            Ok(Err(e)) => Some(format!("Couldn't save settings: {e:#}")),
            Err(_) => None,
        };
        cx.notify();
    }

    /// Change an update setting, then check again with it.
    fn update_updates(&mut self, cx: &mut Context<Self>, change: impl FnOnce(&mut Settings)) {
        self.update_settings(cx, change);
        let _ = self.app.update(cx, |app, cx| {
            if app.settings.check_updates {
                app.check_for_updates(cx);
            }
        });
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
            "This deletes {count} finished run{} and their history. Runs in progress are kept. This can't be undone.",
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
        let panel = cx.entity().downgrade();
        let pages = vec![
            SettingPage::new("Recording")
                .icon(media_icon("video"))
                .description("Applies to runs started from now on.")
                .group(recording_group(&panel)),
            SettingPage::new("Storage")
                .icon(media_icon("hard-drive"))
                .description(self.usage_label())
                .group(self.storage_group(&panel, cx)),
            SettingPage::new("Updates")
                .icon(media_icon("refresh-cw"))
                .group(self.updates_group(&panel, cx)),
        ];
        v_flex()
            .h(px(460.))
            .gap_2()
            .child(
                div().flex_1().min_h_0().child(
                    SettingsView::new("testudo-settings")
                        .sidebar_width(px(180.))
                        .pages(pages),
                ),
            )
            .children(
                self.status
                    .clone()
                    .map(|s| div().px_2().text_sm().text_color(cx.theme().green).child(s)),
            )
    }
}

/// The app's current settings, read through the panel (from a field's value
/// closure, which only gets the `App`).
fn app_settings(panel: &WeakEntity<SettingsPanel>, cx: &App) -> Option<Settings> {
    let app = panel.upgrade()?.read(cx).app.upgrade()?;
    Some(app.read(cx).settings.clone())
}

/// A dropdown setting over `choices`, keyed by position. Changing an update
/// setting checks for updates again.
fn choice<T: Copy + PartialEq + 'static>(
    panel: &WeakEntity<SettingsPanel>,
    choices: Vec<(T, String)>,
    current: impl Fn(&Settings) -> T + 'static,
    set: impl Fn(&mut Settings, T) + Copy + 'static,
    updates: bool,
) -> SettingField<SharedString> {
    let options = choices
        .iter()
        .enumerate()
        .map(|(i, (_, label))| (SharedString::from(i.to_string()), SharedString::from(label.clone())))
        .collect();
    let keys: Rc<Vec<T>> = Rc::new(choices.into_iter().map(|(v, _)| v).collect());
    let (read_panel, read_keys, panel) = (panel.clone(), keys.clone(), panel.clone());
    SettingField::dropdown(
        options,
        move |cx: &App| {
            app_settings(&read_panel, cx)
                .and_then(|s| read_keys.iter().position(|k| *k == current(&s)))
                .unwrap_or(0)
                .to_string()
                .into()
        },
        move |key: SharedString, cx: &mut App| {
            let Some(v) = key.parse::<usize>().ok().and_then(|i| keys.get(i).copied()) else {
                return;
            };
            let _ = panel.update(cx, |p, cx| {
                if updates {
                    p.update_updates(cx, |s| set(s, v));
                } else {
                    p.update_settings(cx, |s| set(s, v));
                }
            });
        },
    )
}

fn hint(text: impl Into<SharedString>, color: Hsla) -> Div {
    div().text_xs().text_color(color).child(text.into())
}

fn recording_group(panel: &WeakEntity<SettingsPanel>) -> SettingGroup {
    let sizes = FRAME_SIZES
        .iter()
        .map(|&px| {
            let label = if px == DEFAULT_FRAME_MAX {
                format!("{px} × {px} px (default)")
            } else {
                format!("{px} × {px} px")
            };
            (px, label)
        })
        .collect();
    let rates = FRAME_RATES.iter().map(|&fps| (fps, rate_label(fps))).collect();
    SettingGroup::new()
        .item(
            SettingItem::new(
                "Resolution",
                choice(panel, sizes, Settings::frame_max, |s, px| s.frame_max_px = px, false),
            )
            .description("Frames fit within this box, keeping the page's shape."),
        )
        .item(
            SettingItem::new(
                "Frame rate",
                choice(panel, rates, Settings::frame_rate, |s, fps| s.frame_rate = fps, false),
            )
            .description("Lower rates save space; a page's last frame is always kept."),
        )
}

impl SettingsPanel {
    fn usage_label(&self) -> String {
        match self.usage {
            None => "Measuring…".to_owned(),
            Some((runs, bytes)) => format!(
                "Using {runs} run{} · {}",
                if runs == 1 { "" } else { "s" },
                fmt_bytes(bytes)
            ),
        }
    }

    fn storage_group(&self, panel: &WeakEntity<Self>, cx: &App) -> SettingGroup {
        let Some(app) = self.app.upgrade() else {
            return SettingGroup::new();
        };
        let settings = app.read(cx).settings.clone();
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
        let custom = settings.recordings_dir.is_some();
        let days = self.older_than_days;

        let location = {
            let panel = panel.clone();
            SettingItem::render(move |_, _, cx| {
                let theme = cx.theme();
                let (muted, warn) = (theme.muted_foreground, theme.yellow);
                let (change, reset) = (panel.clone(), panel.clone());
                v_flex()
                    .w_full()
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
                            .child(button("rec-dir-change").label("Change…").small().outline().on_click(
                                move |_, _, cx| {
                                    let _ = change.update(cx, |p, cx| p.pick_location(cx));
                                },
                            ))
                            .when(custom, |r| {
                                r.child(button("rec-dir-default").label("Default").small().ghost().on_click(
                                    move |_, _, cx| {
                                        let _ = reset
                                            .update(cx, |p, cx| p.update_settings(cx, |s| s.recordings_dir = None));
                                    },
                                ))
                            }),
                    )
                    .when(env_override, |d| {
                        d.child(hint("TESTUDO_DATA_DIR is set and overrides this.", warn))
                    })
                    .when(pending_move, |d| {
                        d.child(hint(
                            "Takes effect the next time Testudo starts. Existing recordings stay where they are.",
                            warn,
                        ))
                    })
            })
        };

        let older =
            {
                let panel = panel.clone();
                SettingField::render(move |_, _, _| {
                    let (pick, delete) = (panel.clone(), panel.clone());
                    h_flex()
                        .gap_2()
                        .child(choice_menu(
                            "older-than",
                            days_label(days),
                            AGE_CHOICES.iter().map(|&d| (days_label(d), d == days, d)),
                            move |d, cx| {
                                let _ = pick.update(cx, |p, cx| {
                                    p.older_than_days = d;
                                    cx.notify();
                                });
                            },
                        ))
                        .child(button("delete-older").label("Delete").small().outline().on_click(
                            move |_, window, cx| {
                                let before = now_ms() - days as f64 * DAY_MS;
                                let _ = delete.update(cx, |p, cx| p.confirm_delete(Some(before), window, cx));
                            },
                        ))
                })
            };

        let delete_all = {
            let panel = panel.clone();
            SettingField::render(move |_, _, _| {
                let panel = panel.clone();
                button("delete-all")
                    .label("Delete all")
                    .small()
                    .danger()
                    .on_click(move |_, window, cx| {
                        let _ = panel.update(cx, |p, cx| p.confirm_delete(None, window, cx));
                    })
            })
        };

        SettingGroup::new()
            .item(location)
            .item(SettingItem::new("Delete recordings older than", older))
            .item(SettingItem::new("Delete all recordings", delete_all).description("Runs in progress are kept."))
    }

    fn updates_group(&self, panel: &WeakEntity<Self>, cx: &App) -> SettingGroup {
        let Some(app) = self.app.upgrade() else {
            return SettingGroup::new();
        };
        let app = app.read(cx);
        let theme = cx.theme();
        let muted = theme.muted_foreground;
        let channel_hint = match app.settings.update_channel {
            UpdateChannel::Stable => "Signed releases from GitHub.",
            UpdateChannel::BleedingEdge => {
                "Every commit on main, built on this machine. Needs git and a Rust toolchain."
            }
        };
        let (update_status, update_color) = match &app.updates.state {
            UpdateState::Idle => (String::new(), muted),
            UpdateState::Checking => ("Checking…".to_owned(), muted),
            UpdateState::UpToDate => ("Up to date.".to_owned(), theme.green),
            UpdateState::CheckFailed(e) => (e.clone(), theme.yellow),
            UpdateState::Available(u) => (format!("{} is available.", u.label), theme.yellow),
            UpdateState::Installing { label, .. } => (format!("Updating to {label}…"), muted),
            UpdateState::Installed(label) => (format!("Updated to {label}. Restart to use it."), theme.green),
            UpdateState::InstallFailed(e) => (format!("Update failed: {e}"), theme.red),
        };
        let not_updatable = app.updates.install.as_ref().err().map(|e| e.to_string());

        let check = {
            let (read_panel, panel) = (panel.clone(), panel.clone());
            SettingField::switch(
                move |cx: &App| app_settings(&read_panel, cx).is_some_and(|s| s.check_updates),
                move |on: bool, cx: &mut App| {
                    let _ = panel.update(cx, |p, cx| p.update_updates(cx, |s| s.check_updates = on));
                },
            )
        };
        let channels = UpdateChannel::ALL
            .iter()
            .map(|&c| (c, channel_label(c).to_owned()))
            .collect();
        let status = {
            let panel = panel.clone();
            SettingItem::render(move |_, _, _| {
                let panel = panel.clone();
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(hint(format!("Testudo {}", testudo_update::current()), muted).flex_none())
                            .child(hint(update_status.clone(), update_color).flex_1().min_w_0())
                            .child(button("check-now").label("Check now").small().outline().on_click(
                                move |_, _, cx| {
                                    let _ = panel.update(cx, |p, cx| {
                                        let _ = p.app.update(cx, |app, cx| app.check_for_updates(cx));
                                    });
                                },
                            )),
                    )
                    .children(not_updatable.clone().map(|why| hint(why, muted)))
            })
        };

        SettingGroup::new()
            .item(SettingItem::new("Check for updates", check).description("At startup and once a day."))
            .item(
                SettingItem::new(
                    "Channel",
                    choice(panel, channels, |s| s.update_channel, |s, c| s.update_channel = c, true),
                )
                .description(channel_hint),
            )
            .item(status)
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

fn channel_label(c: UpdateChannel) -> &'static str {
    match c {
        UpdateChannel::Stable => "Stable",
        UpdateChannel::BleedingEdge => "Bleeding edge",
    }
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
