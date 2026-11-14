//! Update checks, and the banner offering to install one (see testudo-update).

use testudo_update::{Install, NotUpdatable, Update};

use super::*;

const CHECK_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Default)]
pub enum UpdateState {
    #[default]
    Idle,
    Checking,
    UpToDate,
    /// Shown in Settings only: an offline machine shouldn't get a banner a day.
    CheckFailed(String),
    Available(Update),
    Installing {
        label: String,
        line: String,
    },
    Installed(String),
    InstallFailed(String),
}

pub struct Updates {
    pub install: Result<Install, NotUpdatable>,
    pub state: UpdateState,
    /// The banner was closed for this update.
    dismissed: bool,
    /// Bumped per check, so an answer for a channel since changed is dropped.
    generation: u64,
}

impl Updates {
    pub fn new() -> Self {
        Self {
            install: Install::detect(),
            state: UpdateState::Idle,
            dismissed: false,
            generation: 0,
        }
    }

    fn busy(&self) -> bool {
        matches!(self.state, UpdateState::Installing { .. } | UpdateState::Installed(_))
    }
}

enum Progress {
    Line(String),
    Done(Result<(), String>),
}

impl TestudoApp {
    /// Check now and then once a day, while enabled in Settings. Development
    /// builds only check when asked (Settings → Check now).
    pub(super) fn start_update_checks(&mut self, cx: &mut Context<Self>) {
        if cfg!(debug_assertions) {
            return;
        }
        cx.spawn(async move |this, cx| {
            loop {
                let alive = this.update(cx, |app, cx| {
                    if app.settings.check_updates {
                        app.check_for_updates(cx);
                    }
                });
                if alive.is_err() {
                    break;
                }
                cx.background_executor().timer(CHECK_EVERY).await;
            }
        })
        .detach();
    }

    pub(super) fn check_for_updates(&mut self, cx: &mut Context<Self>) {
        if self.updates.busy() {
            return;
        }
        self.updates.generation += 1;
        let generation = self.updates.generation;
        let channel = self.settings.update_channel;
        self.updates.state = UpdateState::Checking;
        cx.notify();
        let check = cx
            .background_executor()
            .spawn(async move { testudo_update::check(channel) });
        cx.spawn(async move |this, cx| {
            let result = check.await;
            let _ = this.update(cx, |app, cx| {
                let u = &mut app.updates;
                if u.generation != generation || u.busy() {
                    return;
                }
                let seen = match &u.state {
                    UpdateState::Available(prev) => Some(prev.label.clone()),
                    _ => None,
                };
                u.state = match result {
                    Ok(Some(update)) => {
                        if seen.as_deref() != Some(update.label.as_str()) {
                            u.dismissed = false;
                        }
                        UpdateState::Available(update)
                    }
                    Ok(None) => UpdateState::UpToDate,
                    Err(e) => {
                        tracing::warn!("update check failed: {e:#}");
                        UpdateState::CheckFailed(format!("Couldn't check for updates: {e:#}"))
                    }
                };
                cx.notify();
            });
        })
        .detach();
    }

    /// Download or build the available update on a thread of its own (a
    /// bleeding-edge build takes minutes), reporting each step in the banner.
    fn install_update(&mut self, cx: &mut Context<Self>) {
        let (UpdateState::Available(update), Ok(install)) = (&self.updates.state, &self.updates.install) else {
            return;
        };
        let (update, install) = (update.clone(), install.clone());
        let label = update.label.clone();
        self.updates.state = UpdateState::Installing {
            label: label.clone(),
            line: "Starting".into(),
        };
        cx.notify();
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        std::thread::spawn(move || {
            let _span = tracing::info_span!("install_update", to = %update.label).entered();
            let result = testudo_update::install(&update, &install, &mut |line| {
                let _ = tx.unbounded_send(Progress::Line(line.to_owned()));
            });
            if let Err(e) = &result {
                tracing::error!("update failed: {e:#}");
            }
            let _ = tx.unbounded_send(Progress::Done(result.map_err(|e| format!("{e:#}"))));
        });
        cx.spawn(async move |this, cx| {
            while let Some(p) = rx.next().await {
                let label = label.clone();
                let alive = this.update(cx, |app, cx| {
                    match p {
                        Progress::Line(l) => {
                            if let UpdateState::Installing { line, .. } = &mut app.updates.state {
                                *line = l;
                            }
                        }
                        Progress::Done(Ok(())) => app.updates.state = UpdateState::Installed(label),
                        Progress::Done(Err(e)) => app.updates.state = UpdateState::InstallFailed(e),
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Start the installed (new) binary on the current project and quit.
    fn restart(&mut self, cx: &mut Context<Self>) {
        let Ok(install) = &self.updates.install else { return };
        let mut cmd = std::process::Command::new(install.exe("testudo"));
        if let Some(dir) = &self.scanned {
            cmd.arg(dir);
        }
        match cmd.spawn() {
            Ok(_) => cx.quit(),
            Err(e) => {
                self.updates.state = UpdateState::InstallFailed(format!("Couldn't start the new version: {e}"));
                cx.notify();
            }
        }
    }

    pub(super) fn render_update_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let bar = || {
            h_flex()
                .gap_2()
                .px_3()
                .py_1()
                .items_center()
                .text_sm()
                .border_b_1()
                .border_color(theme.border)
                .bg(theme.secondary)
        };
        let row = match &self.updates.state {
            UpdateState::Available(u) if !self.updates.dismissed => {
                let url = u.url.clone();
                let what = match u.channel {
                    UpdateChannel::Stable => "Update",
                    UpdateChannel::BleedingEdge => "Build and install",
                };
                bar()
                    .child(format!("Testudo {} is available.", u.label))
                    .child(
                        button("update-notes")
                            .label("What's new")
                            .small()
                            .ghost()
                            .on_click(move |_, _, cx| cx.open_url(&url)),
                    )
                    .child(match &self.updates.install {
                        Ok(_) => button("update-install")
                            .label(what)
                            .small()
                            .primary()
                            .on_click(cx.listener(|this, _, _, cx| this.install_update(cx)))
                            .into_any_element(),
                        Err(why) => div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(why.to_string())
                            .into_any_element(),
                    })
                    .child(div().flex_1())
                    .child(
                        button("update-dismiss")
                            .label("✕")
                            .small()
                            .ghost()
                            .tooltip("Hide until the next update")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.updates.dismissed = true;
                                cx.notify();
                            })),
                    )
            }
            UpdateState::Installing { label, line } => bar().child(format!("Updating to {label}…")).child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .font_family(crate::fonts::mono())
                    .text_color(theme.muted_foreground)
                    .child(line.clone()),
            ),
            UpdateState::Installed(label) => {
                let running = !self.handles.is_empty();
                bar()
                    .child(format!("Updated to {label}. Restart Testudo to use it."))
                    .child(
                        button("update-restart")
                            .label("Restart")
                            .small()
                            .primary()
                            .disabled(running)
                            .when(running, |b| b.tooltip("Stop the running suites first"))
                            .on_click(cx.listener(|this, _, _, cx| this.restart(cx))),
                    )
            }
            UpdateState::InstallFailed(e) => bar()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(theme.red)
                        .child(format!("Update failed: {e}")),
                )
                .child(
                    button("update-failed-dismiss")
                        .label("Dismiss")
                        .small()
                        .ghost()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.updates.state = UpdateState::Idle;
                            cx.notify();
                        })),
                ),
            _ => return None,
        };
        Some(row.into_any_element())
    }
}
