//! The single-page focus view, laid out like a session-replay player: player
//! column (metadata, URL bar, frame, timeline, controls) and a details panel
//! (Steps / Network / Console / Output) synced to the scrub time.

use super::*;
use crate::logic::{status_label, url_path};
use std::sync::Arc;
use testudo_protocol::TestStatus;
use crate::state::{ConsoleFilter, FocusTab, NetFilter};

/// An earlier run's recording beside the frame: (label, status, image).
type ComparedCell = (String, Option<TestStatus>, Option<Arc<RenderImage>>);

/// A step as shown in the focus view.
#[derive(Clone)]
struct StepItem {
    started: f64,
    rel: f64,
    dur: Option<f64>,
    title: SharedString,
    kind: &'static str,
    failed: bool,
}

/// A request as shown in the focus view.
#[derive(Clone)]
struct NetItem {
    started: f64,
    rel: f64,
    finished: Option<f64>,
    method: SharedString,
    status: Option<u16>,
    error: Option<SharedString>,
    kind: SharedString,
    url: SharedString,
    failed: bool,
}

#[derive(Clone)]
struct ConsoleItem {
    at: f64,
    rel: f64,
    level: SharedString,
    text: SharedString,
}

fn step_kind(category: Option<&str>) -> &'static str {
    match category {
        Some("pw:api") => "action",
        Some("expect") => "expect",
        Some("test.step") => "step",
        _ => "other",
    }
}

impl TestudoApp {
    pub(super) fn render_focus(&mut self, w: Which, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let Some(view) = self.view(w) else { return div().into_any_element() };
        let Some(focus) = view.focus.as_ref() else { return div().into_any_element() };
        let model: &Model = focus.recorded.as_deref().unwrap_or(&view.model);
        let Some(page) = model.pages.get(&focus.key) else {
            return div().p_4().child("page no longer available").into_any_element();
        };

        // --- gather ---------------------------------------------------------
        let running = w == Which::Live && page.closed_at.is_none() && focus.recorded.is_none();
        let test = model.test_for_page(page);
        let status = test.and_then(|t| t.status);
        let (_, tn) = status_icon(status);
        let (start, end) = focus.bounds(page);
        let t = focus.t.min(end);
        let span = (end - start).max(1.0);
        let frac = move |x: f64| ((x - start) / span).clamp(0.0, 1.0) as f32;
        let url: SharedString = page
            .navigations
            .iter()
            .rev()
            .find(|(at, _)| *at <= t)
            .map_or(page.url.clone(), |(_, u)| u.clone())
            .into();
        let image = if focus.follow_live || !focus.has_recording() {
            page.frame.clone()
        } else {
            focus.image.clone()
        };
        let no_recording = !focus.follow_live && !focus.has_recording() && !running && self.store.is_some();
        let skipped = (test.and_then(|t| t.status) == Some(testudo_protocol::TestStatus::Skipped))
            .then(|| test.and_then(|t| t.note.clone()).unwrap_or_default());
        let frame_label = (!focus.follow_live)
            .then(|| focus.frame_position())
            .flatten()
            .map(|(i, n)| format!("frame {}/{n}", i + 1));
        let (follow, playing, speed, tab, net_filter, console_filter, sync) = (
            focus.follow_live,
            focus.playing,
            focus.speed,
            focus.tab,
            focus.net_filter,
            focus.console_filter,
            focus.sync_lists,
        );
        let hover_label = focus.hover_label.clone().unwrap_or_default();
        // Earlier runs of this test beside the frame: (label, status, image).
        let by_step = focus.compare_by_step;
        let compare_target = test.and_then(|t| t.file.clone().map(|f| (f, t.title_path.clone())));
        let now = now_ms();
        let compared: Option<Vec<ComparedCell>> = focus.compare.as_ref().map(|lanes| {
            lanes
                .iter()
                .map(|l| {
                    let status = l.prior.status.as_deref().map(|s| match s {
                        "passed" => TestStatus::Passed,
                        "failed" => TestStatus::Failed,
                        "timedOut" => TestStatus::TimedOut,
                        "skipped" => TestStatus::Skipped,
                        "interrupted" => TestStatus::Interrupted,
                        _ => TestStatus::Unknown,
                    });
                    let dur = l.prior.duration_ms.map(|d| format!(" · {}", fmt_dur(d))).unwrap_or_default();
                    let fallback = if l.by_time_fallback { " · steps differ, by time" } else { "" };
                    let label = format!(
                        "{} ({}){dur}{fallback}",
                        fmt_time(l.prior.run_started_at),
                        fmt_ago(l.prior.run_started_at, now)
                    );
                    (label, status, l.image.clone())
                })
                .collect()
        });
        let title = pane_name(model, page).to_owned();
        let browser = page.browser.clone().unwrap_or_default();
        let location = test
            .and_then(|t| t.file.as_ref().map(|f| (f, t.line)))
            .map(|(f, line)| {
                let name = f.rsplit('/').next().unwrap_or(f);
                format!("{name}:{}", line.unwrap_or(0))
            });

        let steps: Rc<Vec<StepItem>> = Rc::new(
            test.map(|t| {
                t.steps
                    .iter()
                    .map(|s| StepItem {
                        started: s.started,
                        rel: (s.started - page.opened_at) / 1000.0,
                        dur: s.ended.map(|e| e - s.started),
                        title: s.title.clone().into(),
                        kind: step_kind(s.category.as_deref()),
                        failed: s.error.is_some(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        );
        // The step running at the scrub time: the latest one that has started.
        let current_step = steps.iter().rposition(|s| s.started <= t);

        let net_all: Vec<NetItem> = page
            .network
            .iter()
            .map(|r| NetItem {
                started: r.started,
                rel: (r.started - page.opened_at) / 1000.0,
                finished: r.finished,
                method: r.method.clone().into(),
                status: r.status,
                error: r.error.clone().map(Into::into),
                kind: r.resource_type.clone().into(),
                url: r.url.clone().into(),
                failed: r.error.is_some() || r.status.is_some_and(|s| s >= 400),
            })
            .collect();
        let counts: Vec<usize> = NetFilter::ALL
            .iter()
            .map(|f| net_all.iter().filter(|r| f.matches(&r.kind, r.failed)).count())
            .collect();
        let search = self.net_search.read(cx).value().to_lowercase();
        let net: Rc<Vec<NetItem>> = Rc::new(
            net_all
                .iter()
                .filter(|r| net_filter.matches(&r.kind, r.failed))
                .filter(|r| search.is_empty() || r.url.to_lowercase().contains(&search))
                .cloned()
                .collect(),
        );
        let current_net = net.iter().rposition(|r| r.started <= t);

        let console: Rc<Vec<ConsoleItem>> = Rc::new(
            page.console
                .iter()
                .filter(|c| match console_filter {
                    ConsoleFilter::All => true,
                    ConsoleFilter::Errors => c.level == "error",
                    ConsoleFilter::Warnings => matches!(c.level.as_str(), "warning" | "warn"),
                })
                .map(|c| ConsoleItem {
                    at: c.at,
                    rel: (c.at - page.opened_at) / 1000.0,
                    level: c.level.clone().into(),
                    text: c.text.clone().into(),
                })
                .collect(),
        );
        let console_total = page.console.len();
        let console_errors = page.console.iter().filter(|c| c.level == "error").count();

        // Timeline marks, as fractions of the page's lifetime.
        let step_bars: Vec<(f32, f32, bool)> = steps
            .iter()
            .map(|s| (frac(s.started), frac(s.started + s.dur.unwrap_or(0.0)), s.failed))
            .collect();
        let ticks: Vec<(f32, bool)> = page.network.iter().map(|r| (frac(r.started), r.error.is_some() || r.status.is_some_and(|s| s >= 400))).collect();
        let navs: Vec<f32> = page.navigations.iter().map(|(at, _)| frac(*at)).collect();
        let errors: Vec<f32> = page.console.iter().filter(|c| c.level == "error").map(|c| frac(c.at)).collect();
        let cursor = frac(t);

        if sync {
            if let Some(i) = current_step {
                self.steps_scroll.scroll_to_item(i, ScrollStrategy::Center);
            }
            if let Some(i) = current_net {
                self.net_scroll.scroll_to_item(i, ScrollStrategy::Center);
            }
        }

        // --- player column -------------------------------------------------
        let status_color = if status.is_none() && !running { muted } else { tone(tn, cx) };
        // Title on its own row, metadata strip beneath (Browserbase-style).
        let header = v_flex()
            .gap_1()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("focus-back")
                            .label("← All pages")
                            .small()
                            .ghost()
                            .tooltip("Esc")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.close_focus(w);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("focus-title")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title.clone())
                            .tooltip(tooltip_text(title.clone())),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(pill(status_label(status), status_color))
                    .when(!browser.is_empty(), |d| d.child(pill(browser, muted)))
                    .child(div().flex_none().text_xs().text_color(muted).child(format!("{:.2}s", (end - start) / 1000.0)))
                    .children(location.map(|l| {
                        div()
                            .min_w_0()
                            .truncate()
                            .text_xs()
                            .font_family(crate::fonts::mono())
                            .text_color(muted)
                            .child(l)
                    }))
                    .child(div().flex_1())
                    .when(self.store.is_some() && compare_target.is_some(), |row| {
                        let comparing = compared.is_some();
                        let align = |id: &'static str, label: &'static str, step: bool, tip: &'static str| {
                            Button::new(id)
                                .label(label)
                                .xsmall()
                                .when(by_step == step, |b| b.primary())
                                .when(by_step != step, |b| b.ghost())
                                .tooltip(tip)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if let Some(f) = this.view_mut(w).and_then(|v| v.focus.as_mut()) {
                                        f.compare_by_step = step;
                                    }
                                    cx.notify();
                                }))
                        };
                        row.when(comparing, |row| {
                            row.child(div().text_xs().text_color(muted).child("Align by"))
                                .child(align(
                                    "f-align-step",
                                    "Step",
                                    true,
                                    "Show each earlier run at the same test step as this one",
                                ))
                                .child(align(
                                    "f-align-time",
                                    "Time",
                                    false,
                                    "Show each earlier run at the same time since its page opened",
                                ))
                        })
                        .child(
                            Button::new("f-compare")
                                .label(if comparing { "Hide comparison" } else { "Compare with previous runs" })
                                .xsmall()
                                .when(comparing, |b| b.primary())
                                .when(!comparing, |b| b.ghost())
                                .tooltip("Play this test's recordings from earlier runs alongside, in step")
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    let store = this.store.clone();
                                    let target = compare_target.clone();
                                    let mut retired = Vec::new();
                                    if let Some(f) = this.view_mut(w).and_then(|v| v.focus.as_mut()) {
                                        if f.compare.is_some() {
                                            retired = f.close_compare();
                                        } else if let (Some(store), Some((file, path))) = (store, target) {
                                            f.open_compare(&file, &path, &store);
                                        }
                                    }
                                    this.retired.extend(retired);
                                    cx.notify();
                                })),
                        )
                    }),
            );

        let copy_url = url.clone();
        let url_bar = h_flex()
            .gap_2()
            .items_center()
            .h(px(30.))
            .px_2()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary)
            .child(div().flex_none().text_xs().text_color(muted).child(if url.starts_with("https") { "🔒" } else { "🌐" }))
            .child(div().flex_1().min_w_0().truncate().text_sm().child(url.clone()))
            .child(
                Button::new("copy-url")
                    .label("Copy")
                    .xsmall()
                    .ghost()
                    .tooltip("Copy URL")
                    .on_click(cx.listener(move |_, _, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(copy_url.to_string()));
                    })),
            );

        let frame = div()
            .id("focus-frame")
            .flex_1()
            .min_h_0()
            .relative()
            .rounded_md()
            .overflow_hidden()
            .border_1()
            .border_color(theme.border)
            .bg(theme.secondary.opacity(0.4))
            .child(match image {
                Some(img_) => img(img_).object_fit(ObjectFit::Contain).size_full().into_any_element(),
                None => div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(muted)
                    .child("No frames recorded for this page")
                    .into_any_element(),
            })
            .when_some(skipped, |d, note| d.child(skip_card(note, &theme)))
            .when(running && follow, |d| {
                d.child(div().absolute().top_2().left_2().child(pill("● LIVE", theme.red).bg(theme.background.opacity(0.8))))
            })
            .when(no_recording, |d| {
                d.child(
                    div()
                        .absolute()
                        .bottom_2()
                        .left_2()
                        .text_xs()
                        .text_color(muted)
                        .child("recording unavailable (disabled or removed by retention); showing last live frame"),
                )
            })
            .on_scroll(cx.listener(move |this, e: &ScrollEvent, _, cx| {
                this.perf.input("focus_frame");
                let d = e.delta.pixel_delta(px(20.));
                let d = f32::from(d.y) + f32::from(d.x);
                let shift = e.modifiers.shift;
                this.with_focus(w, |f, page, store, _| {
                    if d == 0.0 {
                        return;
                    }
                    f.playing = false;
                    if shift {
                        f.wheel_px += d;
                        let steps = (f.wheel_px / WHEEL_PX_PER_FRAME).trunc();
                        if steps != 0.0 {
                            f.wheel_px -= steps * WHEEL_PX_PER_FRAME;
                            f.step_frames(-(steps as i64), page, store);
                        }
                    } else {
                        let (start, end) = f.bounds(page);
                        let dt = -(d as f64 / 50.0) * WHEEL_FRACTION_PER_NOTCH * (end - start);
                        let base = if f.follow_live { end } else { f.t };
                        f.seek(base + dt, page);
                    }
                });
                cx.notify();
            }));

        let bounds_cell = self.focus_scrub.clone();
        let th = theme.clone();
        let timeline = div()
            .id("focus-scrub")
            .w_full()
            .h(px(34.))
            .rounded_md()
            .bg(theme.secondary)
            .cursor_pointer()
            .child(
                canvas(
                    move |bounds, _, _| bounds_cell.set(bounds),
                    move |bounds, _, window, _| {
                        let (x0, y0) = (bounds.left(), bounds.top());
                        let (wd, ht) = (bounds.size.width, bounds.size.height);
                        let x = |f: f32| x0 + wd * f;
                        // Lane 1: steps as bars.
                        for (a, b, failed) in &step_bars {
                            let c = if *failed { th.red } else { th.blue.opacity(0.7) };
                            let width = (wd * (*b - *a)).max(px(2.));
                            window.paint_quad(fill(Bounds::new(point(x(*a), y0 + px(6.)), size(width, px(8.))), c));
                        }
                        // Lane 2: requests as ticks.
                        for (f, bad) in &ticks {
                            let c = if *bad { th.red } else { th.muted_foreground.opacity(0.7) };
                            window.paint_quad(fill(Bounds::new(point(x(*f), y0 + px(19.)), size(px(1.), px(9.))), c));
                        }
                        for f in &navs {
                            window.paint_quad(fill(Bounds::new(point(x(*f), y0 + px(2.)), size(px(2.), ht - px(4.))), th.green));
                        }
                        for f in &errors {
                            window.paint_quad(fill(Bounds::new(point(x(*f) - px(3.), y0 + px(1.)), size(px(6.), px(4.))), th.red));
                        }
                        window.paint_quad(fill(Bounds::new(point(x(cursor) - px(1.), y0), size(px(2.), ht)), th.foreground));
                    },
                )
                .size_full(),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, e: &MouseDownEvent, _, cx| this.scrub_focus(w, e.position.x, cx)),
            )
            .on_mouse_move(cx.listener(move |this, e: &MouseMoveEvent, _, cx| {
                if e.pressed_button == Some(MouseButton::Left) {
                    this.scrub_focus(w, e.position.x, cx);
                } else {
                    this.hover_focus(w, e.position, cx);
                }
            }));

        let controls = h_flex()
            .gap_1()
            .items_center()
            .child(Button::new("f-start").icon(media_icon("skip-back")).small().ghost().tooltip("Start").on_click(cx.listener(
                move |this, _, _, cx| {
                    this.with_focus(w, |f, page, _, _| {
                        let (s, _) = f.bounds(page);
                        f.seek(s, page)
                    });
                    cx.notify();
                },
            )))
            .child(
                Button::new("f-prev")
                    .icon(media_icon("step-back"))
                    .small()
                    .ghost()
                    .tooltip("Previous frame (←, Shift+scroll)")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.with_focus(w, |f, page, store, _| f.step_frames(-1, page, store));
                        cx.notify();
                    })),
            )
            .child(
                Button::new("f-play")
                    .icon(media_icon(if playing { "pause" } else { "play" }))
                    .small()
                    .primary()
                    .rounded(ButtonRounded::Large)
                    .tooltip("Play / pause (Space)")
                    .on_click(cx.listener(|this, _, window, cx| this.on_toggle_play(&TogglePlay, window, cx))),
            )
            .child(
                Button::new("f-next")
                    .icon(media_icon("step-forward"))
                    .small()
                    .ghost()
                    .tooltip("Next frame (→, Shift+scroll)")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.with_focus(w, |f, page, store, _| f.step_frames(1, page, store));
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .px_2()
                    .font_family(crate::fonts::mono())
                    .text_sm()
                    .child(format!("{} / {}", fmt_secs(t - start), fmt_secs(end - start))),
            )
            .child(speed_menu("f-speed", speed, cx, move |this, s| {
                if let Some(f) = this.view_mut(w).and_then(|v| v.focus.as_mut()) {
                    f.speed = s;
                }
            }))
            .children(frame_label.map(|l| div().text_xs().text_color(muted).child(l)))
            .child(div().flex_1().min_w_0().truncate().text_xs().text_color(muted).child(hover_label))
            .when(running, |row| {
                row.child(
                    Button::new("f-live")
                        .label("● LIVE")
                        .xsmall()
                        .when(follow, |b| b.danger())
                        .when(!follow, |b| b.ghost())
                        .tooltip("Follow the page in real time")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(f) = this.view_mut(w).and_then(|v| v.focus.as_mut()) {
                                f.follow_live = true;
                                f.playing = false;
                                f.sync_lists = true;
                            }
                            cx.notify();
                        })),
                )
            });

        let frame: AnyElement = match compared {
            None => frame.into_any_element(),
            Some(lanes) if lanes.is_empty() => v_flex()
                .flex_1()
                .min_h_0()
                .gap_1()
                .child(div().text_xs().text_color(muted).child("No earlier recordings of this test (retention may have removed them)"))
                .child(frame)
                .into_any_element(),
            Some(lanes) => {
                let cell = |label: AnyElement, body: AnyElement| {
                    v_flex().flex_1().min_w_0().min_h_0().gap_1().child(label).child(body)
                };
                let mut cells = vec![cell(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().text_xs().font_weight(FontWeight::SEMIBOLD).child("This run"))
                        .child(pill(status_label(status), status_color))
                        .into_any_element(),
                    frame.into_any_element(),
                )];
                for (label, st, image) in lanes {
                    let (_, stn) = status_icon(st);
                    cells.push(cell(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(div().min_w_0().truncate().text_xs().text_color(muted).child(label))
                            .child(pill(status_label(st), tone(stn, cx)))
                            .into_any_element(),
                        div()
                            .flex_1()
                            .min_h_0()
                            .rounded_md()
                            .overflow_hidden()
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.secondary.opacity(0.4))
                            .children(image.map(|i| img(i).object_fit(ObjectFit::Contain).size_full()))
                            .into_any_element(),
                    ));
                }
                // One row for two recordings, a 2×2 grid beyond that.
                let mut rows = Vec::new();
                let mut it = cells.into_iter();
                while let Some(a) = it.next() {
                    let mut row = h_flex().flex_1().min_h_0().gap_2().child(a);
                    row = match it.next() {
                        Some(b) => row.child(b),
                        None if rows.is_empty() => row,
                        None => row.child(div().flex_1()),
                    };
                    rows.push(row);
                }
                v_flex().flex_1().min_h_0().gap_2().children(rows).into_any_element()
            }
        };

        let player = v_flex()
            .size_full()
            .p_2()
            .gap_2()
            .child(header)
            .child(url_bar)
            .child(frame)
            .child(timeline)
            .child(controls);

        // --- details panel -------------------------------------------------
        let mut tabs = h_flex().gap_1().px_2().py_1().border_b_1().border_color(theme.border);
        for (tb, name, count) in [
            (FocusTab::Steps, "Steps", Some(steps.len())),
            (FocusTab::Network, "Network", Some(net_all.len())),
            (FocusTab::Console, "Console", Some(console_total)),
            (FocusTab::Output, "Output", None),
        ] {
            let label = match count {
                Some(n) => format!("{name} {n}"),
                None => name.to_owned(),
            };
            tabs = tabs.child(
                Button::new(SharedString::from(format!("ftab-{name}")))
                    .label(label)
                    .xsmall()
                    .ghost()
                    .selected(tab == tb)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(f) = this.view_mut(w).and_then(|v| v.focus.as_mut()) {
                            f.tab = tb;
                            f.sync_lists = true;
                        }
                        cx.notify();
                    })),
            );
        }
        if console_errors > 0 {
            let noun = if console_errors == 1 { "error" } else { "errors" };
            tabs = tabs.child(pill(format!("{console_errors} console {noun}"), theme.red));
        }

        let body: AnyElement = match tab {
            FocusTab::Steps => self.focus_steps(w, steps, current_step, t, cx).into_any_element(),
            FocusTab::Network => self.focus_network(w, net, current_net, net_filter, &counts, t, cx).into_any_element(),
            FocusTab::Console => self.focus_console(w, console, console_filter, t, cx).into_any_element(),
            FocusTab::Output => div().size_full().px_2().child(self.render_output(w, window, cx)).into_any_element(),
        };
        let details = v_flex()
            .size_full()
            .border_l_1()
            .border_color(theme.border)
            .child(tabs)
            .child(div().flex_1().min_h_0().child(body));

        if let Some(f) = self.view_mut(w).and_then(|v| v.focus.as_mut()) {
            f.sync_lists = false;
        }

        h_resizable("focus-split")
            .child(resizable_panel().child(player))
            .child(resizable_panel().size(px(480.)).size_range(px(300.)..px(1000.)).child(details))
            .into_any_element()
    }

    fn focus_steps(
        &mut self,
        w: Which,
        steps: Rc<Vec<StepItem>>,
        current: Option<usize>,
        t: f64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        if steps.is_empty() {
            return div()
                .p_3()
                .text_sm()
                .text_color(theme.muted_foreground)
                .child("No steps reported for this page's test.")
                .into_any_element();
        }
        let count = steps.len();
        uniform_list(
            "focus-steps",
            count,
            cx.processor(move |_this, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|i| {
                        let s = &steps[i];
                        let pending = s.started > t;
                        let is_current = current == Some(i);
                        let text = if s.failed { theme.red } else { theme.foreground };
                        let at = s.started;
                        h_flex()
                            .id(("step", i))
                            .w_full()
                            .h(px(24.))
                            .px_2()
                            .gap_2()
                            .items_center()
                            .border_l_2()
                            .border_color(if is_current { theme.blue } else { theme.transparent })
                            .when(is_current, |d| d.bg(theme.list_active))
                            .when(pending, |d| d.opacity(0.4))
                            .cursor_pointer()
                            .hover(|d| d.bg(theme.list_hover))
                            .child(
                                div()
                                    .w(px(62.))
                                    .flex_none()
                                    .font_family(crate::fonts::mono())
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("{:>7.3}s", s.rel)),
                            )
                            .child(div().w(px(60.)).flex_none().child(pill(s.kind, theme.muted_foreground)))
                            .child(div().flex_1().min_w_0().truncate().text_sm().text_color(text).child(s.title.clone()))
                            .child(
                                div()
                                    .w(px(58.))
                                    .flex_none()
                                    .text_right()
                                    .font_family(crate::fonts::mono())
                                    .text_xs()
                                    .text_color(theme.muted_foreground)
                                    .child(s.dur.map(|d| format!("{d:.0}ms")).unwrap_or_default()),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.with_focus(w, |f, page, _, _| f.seek(at, page));
                                cx.notify();
                            }))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.steps_scroll)
        .size_full()
        .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn focus_network(
        &mut self,
        w: Which,
        net: Rc<Vec<NetItem>>,
        current: Option<usize>,
        filter: NetFilter,
        counts: &[usize],
        t: f64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut chips = h_flex().gap_1().flex_wrap();
        for (i, f) in NetFilter::ALL.iter().copied().enumerate() {
            if counts[i] == 0 && f != NetFilter::All && f != filter {
                continue;
            }
            chips = chips.child(
                Button::new(SharedString::from(format!("nf-{}", f.label())))
                    .label(format!("{} {}", f.label(), counts[i]))
                    .xsmall()
                    .ghost()
                    .selected(filter == f)
                    .when(f == NetFilter::Errors && counts[i] > 0, |b| b.danger())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(fc) = this.view_mut(w).and_then(|v| v.focus.as_mut()) {
                            fc.net_filter = f;
                            fc.sync_lists = true;
                        }
                        cx.notify();
                    })),
            );
        }
        let col = |wd: f32| div().w(px(wd)).flex_none();
        let header = h_flex()
            .px_2()
            .gap_2()
            .h(px(22.))
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            .text_xs()
            .text_color(theme.muted_foreground)
            .child(col(62.).child("Time"))
            .child(col(46.).child("Method"))
            .child(col(46.).child("Status"))
            .child(col(66.).child("Type"))
            .child(col(62.).text_right().child("Duration"))
            .child(div().flex_1().child("Path"));
        let count = net.len();
        let list = uniform_list(
            "focus-net",
            count,
            cx.processor(move |_this, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|i| {
                        let r = &net[i];
                        let state = row_state(r.started, r.finished, t);
                        let (status, color) = match state {
                            RowState::Pending => (String::new(), theme.foreground.opacity(0.3)),
                            RowState::InFlight => ("…".to_owned(), theme.blue),
                            RowState::Done => (
                                r.status
                                    .map(|s| s.to_string())
                                    .or_else(|| r.error.as_ref().map(|_| "ERR".to_owned()))
                                    .unwrap_or_default(),
                                if r.failed { theme.red } else { theme.foreground },
                            ),
                        };
                        let dur = match (state, r.finished) {
                            (RowState::Done, Some(f)) => format!("{:.0}ms", f - r.started),
                            _ => String::new(),
                        };
                        let at = r.started;
                        let tip: SharedString = match &r.error {
                            Some(e) => format!("{}\n{e}", r.url).into(),
                            None => r.url.clone(),
                        };
                        let mono = crate::fonts::mono();
                        h_flex()
                            .id(("net-row", i))
                            .w_full()
                            .h(px(22.))
                            .px_2()
                            .gap_2()
                            .items_center()
                            .text_xs()
                            .text_color(color)
                            .border_l_2()
                            .border_color(if current == Some(i) { theme.blue } else { theme.transparent })
                            .when(current == Some(i), |d| d.bg(theme.list_active))
                            .cursor_pointer()
                            .hover(|d| d.bg(theme.list_hover))
                            .child(col(62.).font_family(mono.clone()).child(format!("{:>7.3}s", r.rel)))
                            .child(col(46.).child(r.method.clone()))
                            .child(col(46.).font_family(mono.clone()).child(status))
                            .child(col(66.).truncate().child(r.kind.clone()))
                            .child(col(62.).text_right().font_family(mono).child(dur))
                            .child(div().flex_1().min_w_0().truncate().child(url_path(&r.url).to_owned()))
                            .tooltip(tooltip_text(tip))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.with_focus(w, |f, page, _, _| f.seek(at, page));
                                cx.notify();
                            }))
                    })
                    .collect::<Vec<_>>()
            }),
        )
        .track_scroll(&self.net_scroll)
        .flex_1();

        v_flex()
            .size_full()
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .items_center()
                    .child(div().flex_1().min_w_0().child(chips))
                    .child(div().w(px(170.)).flex_none().child(Input::new(&self.net_search).xsmall())),
            )
            .child(header)
            .child(list)
    }

    fn focus_console(
        &mut self,
        w: Which,
        rows: Rc<Vec<ConsoleItem>>,
        filter: ConsoleFilter,
        t: f64,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let theme = cx.theme().clone();
        let mut chips = h_flex().gap_1().px_2().py_1();
        for (f, name) in [
            (ConsoleFilter::All, "All"),
            (ConsoleFilter::Errors, "Errors"),
            (ConsoleFilter::Warnings, "Warnings"),
        ] {
            chips = chips.child(
                Button::new(SharedString::from(format!("cf-{name}")))
                    .label(name)
                    .xsmall()
                    .ghost()
                    .selected(filter == f)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(fc) = this.view_mut(w).and_then(|v| v.focus.as_mut()) {
                            fc.console_filter = f;
                        }
                        cx.notify();
                    })),
            );
        }
        let count = rows.len();
        v_flex().size_full().child(chips).child(
            uniform_list(
                "focus-console",
                count,
                cx.processor(move |_this, range: std::ops::Range<usize>, _, cx| {
                    range
                        .map(|i| {
                            let c = &rows[i];
                            let base = match c.level.as_ref() {
                                "error" => theme.red,
                                "warning" | "warn" => theme.yellow,
                                _ => theme.foreground,
                            };
                            let at = c.at;
                            h_flex()
                                .id(("console-row", i))
                                .w_full()
                                .min_h(px(22.))
                                .px_2()
                                .gap_2()
                                .items_center()
                                .text_xs()
                                .when(c.at > t, |d| d.opacity(0.35))
                                .cursor_pointer()
                                .hover(|d| d.bg(theme.list_hover))
                                .child(
                                    div()
                                        .w(px(62.))
                                        .flex_none()
                                        .font_family(crate::fonts::mono())
                                        .text_color(theme.muted_foreground)
                                        .child(format!("{:>7.3}s", c.rel)),
                                )
                                .child(div().w(px(64.)).flex_none().child(pill(c.level.clone(), base)))
                                .child(div().flex_1().min_w_0().truncate().text_color(base).child(c.text.clone()))
                                .tooltip(tooltip_text(c.text.clone()))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.with_focus(w, |f, page, _, _| f.seek(at, page));
                                    cx.notify();
                                }))
                        })
                        .collect::<Vec<_>>()
                }),
            )
            .track_scroll(&self.console_scroll)
            .flex_1(),
        )
    }

    pub(super) fn scrub_focus(&mut self, w: Which, x: Pixels, cx: &mut Context<Self>) {
        self.perf.input("focus_timeline");
        let b = self.focus_scrub.get();
        let frac = (f32::from(x - b.left()) / f32::from(b.size.width).max(1.0)).clamp(0.0, 1.0) as f64;
        self.with_focus(w, |f, page, _, _| {
            f.playing = false;
            let (start, end) = f.bounds(page);
            f.seek(start + frac * (end - start), page);
        });
        cx.notify();
    }

    /// Name what is under the mouse on the timeline: a step (top lane), a
    /// request (bottom lane), or a navigation.
    fn hover_focus(&mut self, w: Which, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let b = self.focus_scrub.get();
        let fx = (f32::from(pos.x - b.left()) / f32::from(b.size.width).max(1.0)).clamp(0.0, 1.0) as f64;
        let fy = f32::from(pos.y - b.top());
        let px_ms = |span: f64| span / f64::from(f32::from(b.size.width).max(1.0));
        let Some(view) = self.view(w) else { return };
        let Some(focus) = view.focus.as_ref() else { return };
        let model: &Model = focus.recorded.as_deref().unwrap_or(&view.model);
        let Some(page) = model.pages.get(&focus.key) else { return };
        let (start, end) = focus.bounds(page);
        let at = start + fx * (end - start);
        let slop = 4.0 * px_ms(end - start);
        let rel = |x: f64| (x - page.opened_at) / 1000.0;
        let label = if fy < 16.0 {
            model.test_for_page(page).and_then(|test| {
                test.steps
                    .iter()
                    .rev()
                    .find(|s| s.started - slop <= at && at <= s.ended.unwrap_or(s.started) + slop)
                    .map(|s| {
                        let dur = s.ended.map(|e| format!(", {:.0}ms", e - s.started)).unwrap_or_default();
                        format!("{}  (+{:.3}s{dur})", s.title, rel(s.started))
                    })
            })
        } else {
            page.navigations
                .iter()
                .find(|(t, _)| (t - at).abs() <= slop)
                .map(|(t, u)| format!("navigated to {u}  (+{:.3}s)", rel(*t)))
                .or_else(|| {
                    page.network
                        .iter()
                        .filter(|r| (r.started - at).abs() <= slop)
                        .min_by(|a, b| (a.started - at).abs().total_cmp(&(b.started - at).abs()))
                        .map(|r| {
                            let status = r.status.map(|s| s.to_string()).unwrap_or_else(|| "…".into());
                            format!("{} {status} {}  (+{:.3}s)", r.method, url_path(&r.url), rel(r.started))
                        })
                })
        };
        if let Some(f) = self.view_mut(w).and_then(|v| v.focus.as_mut())
            && f.hover_label != label
        {
            f.hover_label = label;
            cx.notify();
        }
    }
}
