//! The sheets: Resolve (a held update) and the game-build chooser. Everything here reads or
//! acts through the existing workers:
//!
//! * **Resolve** handles the same-base native build’s world stop; Stay writes nothing.
//! * **Build** picks a channel or an exact build. Both use the staged switch.
use super::*;
use crate::frame::button;
use crate::frame::Reach as _;
use crate::paint::palette;
use gpui::{rgb, MouseButton, MouseDownEvent};
use std::sync::mpsc::Sender;

mod provider_choice;
mod publisher_change;
mod switching;
mod versions;
pub(crate) use versions::bench_wheel;

/// More Tab stops than any page has: how far [`Launcher::modal`] looks for the next one inside
/// the layer before it gives up.
const TAB_STOPS: usize = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sheet {
    /// The game-build chooser, a popover under Home's build picker.
    Build,
    /// Choose what to do with a held update.
    Resolve,
    /// A problem's whole text, opened from the play bar's one sentence.
    Notice,
    /// The worlds a data pack goes into (`mods::data_packs`).
    Worlds,
    /// One build to keep, picked from all of them (`versions`).
    Versions,
    Switching,
    SwitchProvider,
    BuildChanges,
    /// Who plays, under the account chip (`account_chip`).
    AccountMenu,
    LauncherUpdate,
    /// The selected profile's options (Sunrise Extras, its folder), a popover under Home's
    /// menu button.
    ProfileMenu,
    Search,
    /// The build window (`mods::build_modal`): closing it leaves the build running.
    ModBuild,
    /// Play was accepted while the registry worker is proving the new generation.
    RegistryPlay,
    GithubAdd,
    NightlySourceLog,
    /// Why an Add or Update was refused (`mods::refused`).
    Refused,
}

/// The way out of a problem the play bar raised, as the Notice sheet's one action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum WayOut {
    /// The game's own files were not found: point Sunrise at the Minecraft folder.
    MinecraftFolder,
    ProfileFolder(std::path::PathBuf),
    /// The build this setup stays on can't be built for any more: follow its channel again.
    Newest(instances::Channel),
}

#[derive(Default)]
pub(crate) struct SheetsState {
    /// Keyboard focus of the layer over the page, so keys never reach the page under it.
    focus: Option<FocusHandle>,
    /// What had the keyboard when that layer opened: it gets it back when the layer closes.
    opener: Option<FocusHandle>,
    /// What has the keyboard while no layer is open, read as each frame starts: the opener of
    /// the next layer, even when the page hands the keyboard to one of that layer's fields.
    before: Option<FocusHandle>,
    pub(super) layered: bool,
    error: Option<String>,
    /// The Resolve sheet shows what the check said word for word.
    details: bool,
    /// What the Notice sheet says, and the way out it offers.
    notice: String,
    /// The notice's own title when it isn't a problem (`open_info`).
    notice_title: Option<String>,
    way_out: Option<WayOut>,
    versions: versions::State,
    pub(super) switching: switching::State,
    publisher_change: Option<publisher_change::PendingPublisherChange>,
    provider_choice: Option<provider_choice::PendingProviderChoice>,
    pub(super) profile_action: Option<crate::profile_delete::Action>,
    /// A build waits on the unreviewed-code warning for native mods nobody reviewed.
    pub(crate) unreviewed: Option<crate::github_mods_page::PendingUnreviewed>,
}

/// A held update as the bottom bar and the Resolve sheet read it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Held {
    /// The build waiting, as people read it ("Nov 17").
    pub(crate) to: String,
    /// The build played now.
    pub(crate) plays: String,
    /// The mods that don't work with it, by id.
    pub(crate) mods: Vec<String>,
    /// What the check or the stopped build said, word for word: for a mod's author, behind
    /// "Details".
    pub(crate) details: Vec<String>,
    /// Worlds that would lose a mod's content.
    pub(crate) worlds: Vec<String>,
    /// A mod the tool refuses on any build: there is nothing to update to.
    pub(crate) blocked: bool,
    /// The raw build the update goes to (`build_native_on`).
    base: String,
    /// It is a build that already stopped for worlds, not a check.
    stopped: bool,
}

/// A channel and version for game builds; the local tag otherwise.
pub(crate) fn build_name(tag: &str) -> String {
    // A game built with mods is named by the build it was made from.
    if native_mods::is_native(tag) {
        if let Some(base) = crate::glass_instance::native_base(tag) {
            return build_name(&base);
        }
    }
    channel_build(tag).map_or_else(
        || tag.to_owned(),
        |(channel, version)| {
            trf(
                "{channel} · {version}",
                &[("channel", channel_name(channel)), ("version", &version)],
            )
        },
    )
}

fn channel_name(channel: instances::Channel) -> &'static str {
    match channel {
        instances::Channel::Nightly => tr("Nightly"),
        instances::Channel::Stable => tr("Stable"),
        instances::Channel::Dev => tr("Dev"),
    }
}

/// The channel name of a dated Nightly or Dev build, which Home's picker shows with its age.
pub(crate) fn dated_channel(tag: &str) -> Option<&'static str> {
    channel_build(tag)
        .filter(|(channel, _)| *channel != instances::Channel::Stable)
        .map(|(channel, _)| channel_name(channel))
}

/// What the build menu calls a channel: the newest build it publishes.
fn latest_name(channel: instances::Channel) -> &'static str {
    match channel {
        instances::Channel::Stable => tr("Latest stable"),
        instances::Channel::Nightly => tr("Latest nightly"),
        instances::Channel::Dev => tr("Latest dev"),
    }
}

/// When a build was published, from its `YYYY-MM-DD` date and `HH:MM` UTC time (empty when
/// unknown): "13 minutes ago", "1 hour ago", "2 days ago" within a week, else "23/11/2026 16:42
/// UTC". Without a time it counts whole days: "today", "yesterday", "2 days ago", else the date.
fn built_when(date: &str, time: &str, now: chrono::DateTime<chrono::Utc>) -> Option<String> {
    use chrono::{NaiveDate, NaiveTime};
    let day = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    let count = |n: i64| n.to_string();
    let Ok(clock) = NaiveTime::parse_from_str(time, "%H:%M") else {
        return Some(match (now.date_naive() - day).num_days() {
            ..=0 => tr("today").into(),
            1 => tr("yesterday").into(),
            days @ 2..=6 => trf("{days} days ago", &[("days", &count(days))]),
            _ => day.format("%d/%m/%Y").to_string(),
        });
    };
    let at = day.and_time(clock).and_utc();
    let ago = now - at;
    Some(match ago.num_minutes() {
        // A clock a little behind the publisher's reads as now, not as the future.
        ..=0 => tr("just now").into(),
        1 => tr("1 minute ago").into(),
        minutes @ 2..=59 => trf("{count} minutes ago", &[("count", &count(minutes))]),
        _ => match ago.num_hours() {
            1 => tr("1 hour ago").into(),
            hours @ 2..=23 => trf("{count} hours ago", &[("count", &count(hours))]),
            _ => match ago.num_days() {
                1 => tr("1 day ago").into(),
                days @ 2..=6 => trf("{days} days ago", &[("days", &count(days))]),
                _ => at.format("%d/%m/%Y %H:%M UTC").to_string(),
            },
        },
    })
}

impl Launcher {
    /// When `tag` was published: the catalogue's time when it lists the build, else the date
    /// in a dated tag.
    pub(crate) fn build_when(&self, tag: &str) -> Option<String> {
        let now = chrono::Utc::now();
        match self.game_releases().iter().find(|r| r.tag == tag) {
            Some(release) => built_when(&release.date, &release.time, now),
            None => backend::dev_date(tag)
                .or_else(|| sunrise_launcher::nightly::date_of(tag))
                .and_then(|date| built_when(date, "", now)),
        }
    }
}

/// The channel an unpinned profile plays now: a Stable profile plays Nightly until Stable has a
/// build ([`instances::Channel::play_target`]), and the picker marks what plays.
fn following_now(saved: instances::Channel, waits_for_stable: bool) -> instances::Channel {
    if waits_for_stable {
        instances::Channel::Nightly
    } else {
        saved
    }
}

/// The version part of the build label, shared with the picker row whose heading is its channel.
fn channel_build(tag: &str) -> Option<(instances::Channel, String)> {
    use instances::Channel;
    use sunrise_launcher::nightly::{date_of, day};
    if let Some(version) = backend::stable_version(tag) {
        Some((Channel::Stable, version.to_owned()))
    } else if let Some(date) = backend::dev_date(tag) {
        Some((Channel::Dev, day(date)))
    } else {
        date_of(tag).map(|date| (Channel::Nightly, day(date)))
    }
}

#[cfg(test)]
#[test]
fn dated_game_builds_share_the_button_and_picker_labels() {
    use instances::Channel;
    let dev = "dev-2026-11-22-7cfdf81";
    let nightly = "nightly-2026-11-20-aaaaaaa";
    assert_eq!(build_name(dev), "Dev · Nov 22");
    assert_eq!(build_name(nightly), "Nightly · Nov 20");
    assert_eq!(build_name("26.3-0.1.0"), "Stable · 0.1.0");
    assert_eq!(channel_build(dev), Some((Channel::Dev, "Nov 22".into())));
    assert_eq!(
        channel_build(nightly),
        Some((Channel::Nightly, "Nov 20".into()))
    );
    assert_eq!(
        channel_build("26.3-0.1.0"),
        Some((Channel::Stable, "0.1.0".into()))
    );
}

fn card() -> Div {
    div()
        .flex()
        .flex_col()
        .rounded(crate::tokens::r(crate::tokens::R_CARD))
        .border_1()
        .border_color(palette().input_edge())
        .bg(rgb(palette().background))
}

fn muted(text: impl Into<SharedString>) -> Div {
    div()
        .text_size(px(tokens::LABEL))
        .text_color(rgb(palette().muted))
        .child(crate::selectable::selectable(text))
}

/// The scrim and the sheet's frame. A press outside closes the sheet; inside, nothing does.
pub(crate) fn frame(
    id: &'static str,
    width: f32,
    body: Div,
    cx: &mut Context<Launcher>,
) -> Stateful<Div> {
    modal_frame(id, width, body, cx, Launcher::close_sheet)
}

pub(crate) fn modal_frame(
    id: &'static str,
    width: f32,
    body: Div,
    cx: &mut Context<Launcher>,
    close: fn(&mut Launcher),
) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .p(px(16.))
        .bg(palette().scrim())
        .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| {
            cx.stop_propagation()
        })
        .on_mouse_down(MouseButton::Middle, |_: &MouseDownEvent, _, cx| {
            cx.stop_propagation()
        })
        .on_scroll(|_, _, cx| cx.stop_propagation())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(move |this, _: &MouseDownEvent, _, cx| {
                close(this);
                cx.notify();
            }),
        )
        .child(
            // Never taller or wider than the window: a small one scrolls the sheet.
            body.id((id, 1usize))
                .w(px(width))
                .max_w_full()
                .max_h_full()
                .overflow_y_scroll()
                .track_scroll(&crate::frame::scrolls("sheet"))
                .p(px(28.))
                .gap(px(16.))
                .rounded(crate::tokens::r(crate::tokens::R_PANEL))
                .border_1()
                // The soft rule the header, the page and the Logs panel use: the raised fill
                // already sets the sheet off the scrim, so a bright edge only shouts.
                .border_color(rgb(palette().divider))
                .bg(rgb(palette().raised))
                .text_color(rgb(palette().text))
                .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                    cx.stop_propagation()
                }),
        )
}

/// A prominent warning modal frame: strong warning border, edge-to-edge warning header band
/// supplied by `body`, and press-outside closes just like [`frame`].
pub(crate) fn warning_frame(
    id: &'static str,
    width: f32,
    border: Hsla,
    body: Div,
    cx: &mut Context<Launcher>,
) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .occlude()
        .flex()
        .items_center()
        .justify_center()
        .p(px(16.))
        .bg(palette().scrim())
        .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| {
            cx.stop_propagation()
        })
        .on_mouse_down(MouseButton::Middle, |_: &MouseDownEvent, _, cx| {
            cx.stop_propagation()
        })
        .on_scroll(|_, _, cx| cx.stop_propagation())
        .on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, _: &MouseDownEvent, _, cx| {
                this.close_sheet();
                cx.notify();
            }),
        )
        .child(
            body.id((id, 1usize))
                .w(px(width))
                .max_w_full()
                .max_h_full()
                .overflow_y_scroll()
                .track_scroll(&crate::frame::scrolls("sheet"))
                .rounded(crate::tokens::r(crate::tokens::R_PANEL))
                .border_2()
                .border_color(border)
                .bg(rgb(palette().raised))
                .text_color(rgb(palette().text))
                .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                    cx.stop_propagation()
                }),
        )
}

pub(crate) fn title(text: String) -> Div {
    div()
        .text_size(px(tokens::TITLE))
        .font_weight(FontWeight::BOLD)
        .child(crate::selectable::selectable(text))
}

/// Moves the keyboard to the next Tab stop inside the layer that `focus` holds, or the one
/// before it (`back`); the page's own stops are passed over.
fn step_inside(focus: &FocusHandle, back: bool, window: &mut Window, cx: &mut App) {
    for _ in 0..TAB_STOPS {
        if back {
            window.focus_prev(cx);
        } else {
            window.focus_next(cx);
        }
        if focus.contains_focused(window, cx) {
            break;
        }
    }
    if !focus.contains_focused(window, cx) {
        window.focus(focus, cx);
    }
}

impl Launcher {
    /// The arrow keys in a layer that is a menu: the row below, or the one above (`back`).
    pub(crate) fn step_in_layer(&self, back: bool, window: &mut Window, cx: &mut App) {
        if let Some(focus) = &self.ui.sheets.focus {
            step_inside(focus, back, window, cx);
        }
    }

    /// A shortcut must not steal focus or dispatch an action behind a consent or modal.
    pub(crate) fn layer_open(&self) -> bool {
        self.ui.sheets.layered || self.ui.sheet.is_some() || self.blocking_dialog()
    }

    /// These dialogs can arrive over an already-open search. Its shortcut must not cancel
    /// another dialog's answer merely because the search is still underneath it.
    pub(crate) fn blocking_dialog(&self) -> bool {
        self.ui.sheets.publisher_change.is_some()
            || self.ui.sheets.provider_choice.is_some()
            || self.ui.sheets.unreviewed.is_some()
            || self.source_question().is_some()
            || self.mods.installs.question.is_some()
            || self.mods.installs.world.is_some()
            || self.sign_in_screen.open
            || self.instances.dialog.is_some()
            || self.ui.sheets.profile_action.is_some()
            || (self.ui.page == crate::frame::Page::Hosting && self.servers.create.is_some())
    }

    /// Opens `sheet`. A pending question opens its own sheet.
    pub(crate) fn open_sheet(&mut self, sheet: Sheet) {
        self.ui.sheets.error = None;
        self.ui.sheets.details = false;
        self.ui.sheet = Some(sheet);
    }

    /// Opens a problem's whole text as a sheet, with the way out of it when there is one.
    pub(crate) fn open_notice(&mut self, text: String, way_out: Option<WayOut>) {
        self.open_sheet(Sheet::Notice);
        self.ui.sheets.notice = text;
        self.ui.sheets.notice_title = None;
        self.ui.sheets.way_out = way_out;
    }

    /// The notice sheet for something to read that isn't a problem, under `title`.
    pub(crate) fn open_info(&mut self, title: String, text: String) {
        self.open_notice(text, None);
        self.ui.sheets.notice_title = Some(title);
    }

    pub(crate) fn reset_install_question_sheet(&mut self) {
        self.ui.sheets.details = false;
        self.ui.sheets.error = None;
    }

    /// Closes the sheet on screen the way Escape or a press outside does. Nothing is allowed,
    /// built or changed by it: a pending question is answered no, a held update stays held.
    pub(crate) fn close_sheet(&mut self) {
        if self.ui.sheet == Some(Sheet::GithubAdd) {
            self.github.close();
        }
        if self.ui.sheet == Some(Sheet::Switching) {
            self.cancel_switch();
        }
        if self.ui.sheet == Some(Sheet::SwitchProvider) {
            self.cancel_provider_switch();
        }
        if let Some(pending) = self.ui.sheets.unreviewed.take() {
            pending.refuse();
        } else if self.dismiss_publisher_change() || self.answer_provider_choice(None) {
            // Keep the current version and remembered publisher; install no provider.
        } else if self.source_question().is_some() {
            self.answer_source_question(Ok(false));
        } else if let Some(reply) = self
            .mods
            .installs
            .question
            .take()
            .map(|(_, reply)| reply)
            .or_else(|| self.mods.installs.world.take().map(|(_, reply)| reply))
        {
            let _ = reply.send(false);
        }
        self.ui.sheet = None;
        self.ui.sheets.error = None;
    }

    /// Escape: true when a sheet was open and is closed now.
    pub(crate) fn sheet_escape(&mut self) -> bool {
        let prompt_open = self.ui.sheets.publisher_change.is_some()
            || self.ui.sheets.provider_choice.is_some()
            || self.ui.sheets.unreviewed.is_some()
            || self.source_question().is_some()
            || self.mods.installs.question.is_some()
            || self.mods.installs.world.is_some();
        if !prompt_open && self.ui.sheets.profile_action.is_some() {
            self.close_profile_action();
            return true;
        }
        let open = prompt_open || self.ui.sheet.is_some();
        if open {
            self.close_sheet();
        }
        open
    }

    /// A same-base native build can still stop for worlds outside of Switch.
    pub(crate) fn held_update(&self) -> Option<Held> {
        let instance = self.selected_instance()?;
        let plays = self.version_of(instance)?;
        let stake = self
            .native_worlds
            .as_ref()
            .filter(|_| self.native_for.as_deref() == Some(instance.id.as_str()))?;
        Some(Held {
            to: build_name(&stake.base),
            plays: build_name(plays),
            mods: stake.mods.clone(),
            details: stake.text.split("\n\n").map(str::to_owned).collect(),
            worlds: stake.worlds.clone(),
            blocked: false,
            base: stake.base.clone(),
            stopped: true,
        })
    }

    /// Reads what has the keyboard, before this frame moves it: see [`Self::modal`].
    pub(crate) fn modal_before(&mut self, window: &Window, cx: &App) {
        if !self.ui.sheets.layered {
            self.ui.sheets.before = window.focused(cx);
        }
    }

    /// The one focus rule for anything drawn over the page (a sheet, Host's Create, the
    /// sign-in): `layer` takes the keyboard when it opens, Tab stays inside it, and when it
    /// closes, by Escape or any other way, the keyboard goes back to what opened it. Escape
    /// itself is the frame's, which knows what closing each layer means.
    pub(crate) fn modal(
        &mut self,
        layer: Option<Stateful<Div>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let Some(layer) = layer else {
            self.ui.sheets.layered = false;
            if let Some(opener) = self.ui.sheets.opener.take() {
                // What opened it may have left the page since: then the frame has the keyboard.
                let still_there = self.root_focus.contains(&opener, window);
                window.focus(
                    if still_there {
                        &opener
                    } else {
                        &self.root_focus
                    },
                    cx,
                );
            }
            return None;
        };
        let focus = self
            .ui
            .sheets
            .focus
            .get_or_insert_with(|| cx.focus_handle())
            .clone();
        if !self.ui.sheets.layered {
            self.ui.sheets.layered = true;
            self.ui.sheets.opener = self.ui.sheets.before.take();
        }
        // The keyboard is somewhere on the page under the layer: the layer takes it. A field
        // the page focused for this layer, not drawn until this frame, keeps it.
        if !focus.contains_focused(window, cx) && self.root_focus.contains_focused(window, cx) {
            window.focus(&focus, cx);
        }
        Some(
            layer
                .track_focus(&focus)
                .seen()
                .on_key_down(move |ev: &KeyDownEvent, window, cx| {
                    if ev.keystroke.key != "tab" {
                        return;
                    }
                    step_inside(&focus, ev.keystroke.modifiers.shift, window, cx);
                    cx.stop_propagation();
                })
                .into_any_element(),
        )
    }

    /// The sheet on screen, if any: a pending question, then the sheet asked for.
    pub(crate) fn live_sheet(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Stateful<Div>> {
        if let Some(sheet) = self.unreviewed_build_sheet(window, cx) {
            return Some(sheet);
        }
        if let Some(pending) = &self.ui.sheets.publisher_change {
            return Some(self.publisher_change_sheet(&pending.question, cx));
        }
        if let Some(pending) = &self.ui.sheets.provider_choice {
            return Some(self.provider_choice_sheet(&pending.need, cx));
        }
        if let Some(body) = self.source_consent_body(cx) {
            return Some(frame("nightly-source-consent", 660., body, cx));
        }
        if let Some((failure, _)) = &self.mods.installs.question {
            return Some(self.registry_build_failed(failure, cx));
        }
        if let Some((impact, _)) = &self.mods.installs.world {
            let action = if impact.removal {
                tr("Remove")
            } else {
                tr("Update")
            };
            let (cancel, approve) = if impact.cancel_default {
                (
                    crate::frame::primary("cancel-registry-change", tr("Cancel")),
                    button("approve-registry-change", action),
                )
            } else {
                (
                    button("cancel-registry-change", tr("Cancel")),
                    crate::frame::primary("approve-registry-change", action),
                )
            };
            let body = div()
                .flex()
                .flex_col()
                .child(title(impact.heading.clone()))
                .when(!impact.detail.is_empty(), |body| {
                    body.child(
                        div()
                            .id("registry-impact-text")
                            .max_h(px(310.))
                            .overflow_y_scroll()
                            .child(crate::selectable::selectable(impact.detail.clone())),
                    )
                })
                .child(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.))
                        .child(cancel.on_click(cx.listener(|this, _, _, cx| {
                            this.close_sheet();
                            cx.notify();
                        })))
                        .child(approve.on_click(cx.listener(|this, _, _, cx| {
                            if let Some((_, answer)) = this.mods.installs.world.take() {
                                let _ = answer.send(true);
                            }
                            cx.notify();
                        }))),
                );
            return Some(frame("registry-world-impact", 690., body, cx));
        }
        if let Some(profile_action) = self.profile_action_sheet(cx) {
            return Some(profile_action);
        }
        match self.ui.sheet {
            Some(Sheet::Build) => Some(self.build_sheet(cx)),
            Some(Sheet::Notice) => Some(self.notice_sheet(cx)),
            Some(Sheet::GithubAdd) => Some(self.github_add_sheet(window, cx)),
            Some(Sheet::Refused) => {
                let sheet = self.refused_sheet(cx);
                if sheet.is_none() {
                    self.ui.sheet = None;
                }
                sheet
            }
            Some(Sheet::NightlySourceLog) => self
                .source_log_body(cx)
                .map(|body| frame("nightly-source-log", 690., body, cx)),
            Some(Sheet::Versions) => Some(self.versions_sheet(cx)),
            Some(Sheet::Switching) => Some(self.switch_sheet(cx)),
            Some(Sheet::SwitchProvider) => self.provider_switch_sheet(cx),
            Some(Sheet::BuildChanges) => self.switch_receipt_sheet(cx),
            Some(Sheet::AccountMenu) => self.account_menu(cx),
            Some(Sheet::LauncherUpdate) => self.launcher_update_sheet(cx),
            Some(Sheet::ProfileMenu) => Some(self.profile_menu(cx)),
            Some(Sheet::Search) => Some(self.command_search(cx)),
            Some(Sheet::RegistryPlay) => match self.registry_stage() {
                Some(stage) => {
                    use crate::mods_page::install::{stage_fraction, stage_label};
                    let body = div()
                        .flex()
                        .flex_col()
                        .child(title(tr("Getting ready to play").into()))
                        .child(muted(crate::eta::with_eta(
                            stage_label(stage),
                            self.registry_eta(),
                        )))
                        .children(
                            stage_fraction(stage)
                                .map(|f| crate::widgets::progress_bar(Some((f, f)), &self.theme)),
                        )
                        .child(div().flex().justify_end().child(
                            button("hide-registry-play", tr("Keep browsing")).on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.close_sheet();
                                    cx.notify();
                                }),
                            ),
                        ));
                    Some(frame("registry-play", 550., body, cx))
                }
                None => {
                    self.ui.sheet = None;
                    None
                }
            },
            Some(Sheet::ModBuild) => {
                let sheet = self.build_modal(cx);
                if sheet.is_none() {
                    self.ui.sheet = None;
                }
                sheet
            }
            Some(Sheet::Worlds) => {
                let sheet = self.world_pick_sheet(cx);
                if sheet.is_none() {
                    self.ui.sheet = None;
                }
                sheet
            }
            Some(Sheet::Resolve) => match self.held_update() {
                Some(held) => Some(self.resolve_sheet(held, cx)),
                None => {
                    // Nothing is held any more (it was resolved elsewhere): close quietly.
                    self.ui.sheet = None;
                    None
                }
            },
            None => None,
        }
    }

    fn registry_build_failed(
        &self,
        failure: &sunrise_launcher::browser::sunrise::BuildNotice,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let show = self.ui.sheets.details;
        let mut list = div()
            .id("registry-install-disposition")
            .flex()
            .flex_col()
            .max_h(px(222.))
            .overflow_y_scroll()
            .rounded(crate::tokens::r(crate::tokens::R_CARD))
            .border_1()
            .border_color(rgb(palette().divider));
        for (i, change) in failure.changes.iter().enumerate() {
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.))
                    .px(px(12.))
                    .py(px(7.))
                    .when(i > 0, |row| {
                        row.border_t_1().border_color(rgb(palette().divider))
                    })
                    .child(
                        div()
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .child(change.title.clone()),
                    )
                    .child(muted(trf(
                        "{disposition} · {reason}",
                        &[
                            ("disposition", &change.disposition()),
                            ("reason", &change.reason.short_reason()),
                        ],
                    )))
                    .children(show.then(|| muted(change.reason.detail()))),
            );
        }
        let actions = show.then(|| {
            let mut controls = div().flex().gap(px(8.)).items_center();
            if let Some(log) = &failure.log {
                let copy = log.clone();
                let save = log.clone();
                controls = controls
                    .child(
                        button("install-log-copy", tr("Copy")).on_click(move |_, _, cx| {
                            crate::widgets::copy_to_clipboard(
                                cx,
                                gpui::ClipboardItem::new_string(copy.to_string()),
                            );
                        }),
                    )
                    .child(
                        button("install-log-save", tr("Save log")).on_click(cx.listener(
                            move |this, _, _, cx| {
                                this.save_install_log(save.clone(), cx);
                            },
                        )),
                    );
            } else {
                controls = controls.child(muted(tr("No build log was saved for this attempt.")));
            }
            controls
        });
        let body = div().flex().flex_col()
            .child(title(tr("Some mods couldn't be built").into()))
            .child(muted(tr("Stay to leave all your mods unchanged. Continue to install the checked set with the changes below.")))
            .child(list)
            .children(actions)
            .children(self.ui.sheets.error.as_ref().map(|error| muted(error.clone())))
            .child(div().flex().justify_end().gap(px(8.))
                .child(button("install-show-why", if show { tr("Hide why") } else { tr("Show why") })
                    .on_click(cx.listener(|this, _, _, cx| { this.ui.sheets.details ^= true; cx.notify(); })))
                .child(crate::frame::primary("install-stay", tr("Stay"))
                    .on_click(cx.listener(|this, _, _, cx| { this.close_sheet(); cx.notify(); })))
                .child(button("install-continue", tr("Continue"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some((_, answer)) = this.mods.installs.question.take() { let _ = answer.send(true); }
                        this.ui.sheets.details = false;
                        this.ui.sheets.error = None;
                        cx.notify();
                    }))));
        frame("registry-build-failed", 690., body, cx)
    }

    fn save_install_log(&mut self, log: std::sync::Arc<str>, cx: &mut Context<Self>) {
        let directory = dirs::download_dir()
            .or_else(dirs::home_dir)
            .unwrap_or_else(backend::data_dir);
        let save = cx.prompt_for_new_path(&directory, Some("sunrise-build.log"));
        cx.spawn(async move |this, cx| {
            let result = match save.await {
                Ok(Ok(Some(path))) => {
                    cx.background_executor()
                        .spawn(async move {
                            std::fs::write(path, log.as_bytes()).map_err(|e| e.to_string())
                        })
                        .await
                }
                Ok(Ok(None)) => return,
                Ok(Err(error)) => Err(error.to_string()),
                Err(_) => return,
            };
            let _ = this.update(cx, |this, cx| {
                if this.mods.installs.question.is_some() {
                    this.ui.sheets.error = Some(match result {
                        Ok(()) => tr("Build log saved.").into(),
                        Err(reason) => trf(
                            "Couldn't save the build log: {reason}",
                            &[("reason", &reason)],
                        ),
                    });
                    cx.notify();
                }
            });
        })
        .detach();
    }

    // ---- Resolve ----

    /// An update is out and some mods don't work with it yet: which, and the two ways on.
    /// Staying is the default and what closing does; nothing here runs until a button is pressed.
    fn resolve_sheet(&mut self, held: Held, cx: &mut Context<Self>) -> Stateful<Div> {
        let Held {
            to,
            plays,
            mods,
            details,
            worlds,
            blocked,
            base,
            ..
        } = held;
        let known = self.mods_named(&mods);
        let names: Vec<String> = known.iter().map(|(name, _)| name.clone()).collect();
        let them = match names.as_slice() {
            [one] => one.clone(),
            _ => tr("them").to_string(),
        };
        let heading = match (blocked, names.as_slice()) {
            (true, _) => trf("{to} is out, but one of your mods stops it", &[("to", &to)]),
            (false, [one]) => trf(
                "{to} is out, but {mod} doesn't work with it yet",
                &[("to", &to), ("mod", one)],
            ),
            (false, many) => trf(
                "{to} is out, but {count} of your mods don't work with it yet",
                &[("to", &to), ("count", &many.len().to_string())],
            ),
        };
        let lead = if blocked {
            trf(
                "One of your mods can't be built for any game build. Remove it under Mods, or wait for its author. Until then you keep playing {plays}.",
                &[("plays", &plays)],
            )
        } else {
            trf(
                "You can keep playing {plays} and wait, or update without {them}.",
                &[("plays", &plays), ("them", &them)],
            )
        };

        // The mods, each with what can be done about it right here.
        let mut can_update = false;
        let waiting = known.iter().filter(|(_, newer)| newer.is_none()).count();
        let mut list = div()
            .id("alt-resolve-list")
            .flex()
            .flex_col()
            .max_h(px(280.))
            .overflow_y_scroll()
            .rounded(crate::tokens::r(crate::tokens::R_CARD))
            .border_1()
            .border_color(palette().input_edge())
            .bg(rgb(palette().background));
        for (n, (id, (name, newer))) in mods.iter().zip(&known).enumerate() {
            // Only a mod that can do something here gets a line under its name; the ones that
            // wait for their author are said once, under the list.
            let about = newer.as_ref().map(|version| {
                trf(
                    "Version {version} is out. Update it, then check the game update again.",
                    &[("version", version)],
                )
            });
            let update = newer.as_ref().map(|_| {
                can_update = true;
                let id = id.clone();
                button(
                    ElementId::named_usize("alt-resolve-mod", n),
                    trf("Update {mod}", &[("mod", name)]),
                )
                .bg(rgb(palette().text))
                .border_color(rgb(palette().text))
                .text_color(rgb(palette().background))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.close_sheet();
                    this.update_mod(&id);
                    cx.notify();
                }))
            });
            list = list.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.))
                    .px(px(14.))
                    .py(px(8.))
                    .when(n > 0, |d| d.border_t_1().border_color(rgb(palette().line)))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(2.))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(name.clone()))
                            .children(about.map(muted)),
                    )
                    .children(update),
            );
        }

        // The mods with no newer version, said once for all of them.
        let wait_line = (waiting > 0).then(|| {
            muted(match waiting {
                1 => tr("It needs an update from its author; when one is out, it shows here and under Mods.").to_string(),
                _ => tr("They need updates from their authors; when one is out, it shows here and under Mods.").to_string(),
            })
        });

        // What updating without them does, said before the button that does it.
        let without = (!blocked).then(|| {
            let kept = trf(
                "Updating without {them} turns it off; it isn't removed, and you can turn it back on when it fits.",
                &[("them", &them)],
            );
            match worlds.as_slice() {
                [] => kept,
                saved => format!(
                    "{kept} {}",
                    trf(
                        "{worlds}: opened without the mod, a world loses what the mod added to it, for good. Back it up first in the game (Edit World, then Make Backup) if you want to keep it.",
                        &[("worlds", &saved.join(", "))],
                    )
                ),
            }
        });

        let stay = button(
            "alt-resolve-stay",
            trf("Stay on {build} for now", &[("build", &plays)]),
        )
        // Recommended, unless a mod can simply be updated.
        .when(!can_update, |b| {
            b.bg(rgb(palette().text))
                .border_color(rgb(palette().text))
                .text_color(rgb(palette().background))
        })
        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
            // Staying is what already happens: nothing is written.
            this.close_sheet();
            cx.notify();
        }));
        let go = (!blocked).then(|| {
            button(
                "alt-resolve-update",
                trf("Update without {them}", &[("them", &them)]),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.close_sheet();
                this.build_native_on(Some(base.clone()), true);
                cx.notify();
            }))
        });

        let open = self.ui.sheets.details;
        let more = div()
            .flex()
            .flex_col()
            .gap(px(8.))
            .child(
                div()
                    .id("alt-resolve-details")
                    .tab_index(0)
                    .seen()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .text_size(px(tokens::SMALL))
                    .text_color(rgb(palette().muted))
                    .cursor_pointer()
                    .child(crate::glass_ui::disclosure_chevron(
                        "resolve-details",
                        open,
                        11.,
                        rgb(palette().muted).into(),
                    ))
                    .child(tr("Details"))
                    .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                        this.ui.sheets.details = !this.ui.sheets.details;
                        cx.notify();
                    })),
            )
            .children(open.then(|| {
                crate::code_box::code_box(
                    "alt-resolve-details-box",
                    details.into_iter().map(Into::into).collect(),
                    None,
                )
                .border_0()
                .text_size(px(tokens::CAPTION))
            }));

        let body = div()
            .flex()
            .flex_col()
            .child(title(heading))
            .child(div().text_size(px(tokens::BODY)).child(lead))
            .children((!mods.is_empty()).then_some(list))
            .children(wait_line)
            .children(without.map(muted))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap(px(8.))
                    .children(go)
                    .child(stay),
            )
            .child(more);
        frame("alt-resolve", 580., body, cx)
    }

    /// What went wrong, in full: each paragraph on its own, with one way out.
    fn notice_sheet(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        let paragraphs: Vec<Div> = self
            .ui
            .sheets
            .notice
            .split('\n')
            .filter(|p| !p.trim().is_empty())
            .map(|p| div().text_size(px(tokens::LABEL)).child(p.to_owned()))
            .collect();
        let main = |label: SharedString| crate::frame::primary("alt-notice-way-out", label);
        let way_out = self.ui.sheets.way_out.clone().map(|way| match way {
            WayOut::MinecraftFolder => main(tr("Choose Minecraft folder").into()).on_click(
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    this.close_sheet();
                    this.choose_folder(cx);
                    cx.notify();
                }),
            ),
            WayOut::ProfileFolder(home) => {
                main(tr("Open folder").into()).on_click(move |_, _, cx| {
                    glass_toast::open(cx, &home);
                })
            }
            WayOut::Newest(channel) => main(match channel {
                instances::Channel::Stable => tr("Use the newest Stable").into(),
                instances::Channel::Nightly => tr("Use the newest Nightly").into(),
                instances::Channel::Dev => tr("Use the newest Dev").into(),
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.native_error = None;
                // The chooser opens under Home's build picker if this switch is canceled.
                this.navigate(crate::frame::Page::Home);
                this.open_sheet(Sheet::Build);
                this.switch_channel(channel);
                cx.notify();
            })),
        });
        let body = div()
            .flex()
            .flex_col()
            .child(title(
                self.ui
                    .sheets
                    .notice_title
                    .clone()
                    .unwrap_or_else(|| tr("What went wrong").into()),
            ))
            .child(div().flex().flex_col().gap(px(10.)).children(paragraphs))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap(px(8.))
                    .child(
                        button("alt-notice-close", tr("Close")).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.close_sheet();
                                cx.notify();
                            },
                        )),
                    )
                    .children(way_out),
            );
        frame("alt-notice", 560., body, cx)
    }

    // ---- Game build ----

    /// The latest published build, or the latest installed one when the channel is offline.
    fn channel_switch_target(&self, channel: instances::Channel) -> Option<&String> {
        channel
            .builds(self.game_releases())
            .next()
            .map(|release| &release.tag)
            .or_else(|| match channel {
                instances::Channel::Nightly => self.known_nightly.as_ref(),
                _ => channel.newest_installed(&self.installed),
            })
    }

    fn switch_channel(&mut self, channel: instances::Channel) {
        use sunrise_launcher::browser::sunrise::BuildChoice;
        let Some(target) = self.channel_switch_target(channel).cloned() else {
            return;
        };
        self.begin_switch(target, Some(BuildChoice::Follow(channel)));
    }

    fn build_sheet(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        let Some(instance) = self.selected_instance() else {
            return div().id("alt-build");
        };
        let pinned = instance.version.clone();
        let plays = self.version_of(instance).cloned();
        let status = if pinned.is_some() {
            Some(tr("Stays on this build"))
        } else if plays.is_none() {
            let signed_in = self.accounts.selected().is_some()
                || sunrise_launcher::developer_offline::selected();
            Some(if signed_in {
                tr("Not installed yet")
            } else {
                tr("Installs after you sign in")
            })
        } else {
            None
        };
        let playing_target = self.profile_target(instance);
        let active_channel = playing_target
            .as_deref()
            .and_then(instances::Channel::of_build);
        let waits_for_stable =
            instance.channel == instances::Channel::Stable && !self.stable_available();
        let follows = pinned
            .is_none()
            .then_some(following_now(instance.channel, waits_for_stable));
        let is_pack = instance.kind == instances::Kind::Pack;
        let option = |id: &'static str, on: bool, head: String, sub: String| {
            div()
                .id(id)
                .reach()
                .flex()
                .gap(px(12.))
                .p(px(10.))
                .rounded(crate::tokens::r(crate::tokens::R_CONTROL))
                .cursor_pointer()
                .when(on, |d| d.bg(rgb(palette().selected)))
                .child(
                    div()
                        .mt(px(3.))
                        .size(px(14.))
                        .flex_none()
                        .rounded_full()
                        .border_1()
                        .border_color(rgb(if on {
                            palette().accent
                        } else {
                            palette().muted
                        }))
                        .when(on, |d| d.bg(rgb(palette().accent))),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(div().text_size(px(tokens::BODY)).child(head))
                        // Plain text: the whole option is the button, nothing in it selects.
                        .child(
                            div()
                                .text_size(px(tokens::SMALL))
                                .text_color(rgb(palette().muted))
                                .child(sub),
                        ),
                )
        };
        use instances::Channel;
        let mut list = div().flex().flex_col().gap(px(2.));
        for (id, channel, unavailable) in [
            (
                "alt-build-stable",
                Channel::Stable,
                tr("No Stable build yet."),
            ),
            (
                "alt-build-nightly",
                Channel::Nightly,
                tr("No Nightly build yet."),
            ),
            ("alt-build-dev", Channel::Dev, tr("No Dev build yet.")),
        ] {
            let selected = follows == Some(channel);
            // Updating a channel already in use is the job of Update game, not this row.
            let current = selected && active_channel == Some(channel) && plays.is_some();
            let tag = if current {
                playing_target.as_ref()
            } else {
                self.channel_switch_target(channel)
            };
            // Stable says its version; every row says when its build came out.
            let version = tag.and_then(|tag| {
                let (_, version) = channel_build(tag)?;
                Some(match (channel, self.build_when(tag)) {
                    (Channel::Stable, Some(when)) => trf(
                        "{version} · {when}",
                        &[("version", &version), ("when", &when)],
                    ),
                    (Channel::Stable, None) => version,
                    (_, when) => when.unwrap_or(version),
                })
            });
            let unavailable_row = is_pack || version.is_none();
            // A Stable profile plays Nightly until Stable has a build; say so on the row it
            // will move to, rather than marking a row that can't play.
            let unavailable = if channel == Channel::Stable && waits_for_stable && pinned.is_none()
            {
                tr("No Stable build yet. Plays Nightly until there is one.")
            } else {
                unavailable
            };
            list = list.child(
                option(
                    id,
                    selected,
                    latest_name(channel).into(),
                    version.unwrap_or_else(|| unavailable.into()),
                )
                .when(unavailable_row, |d| d.opacity(0.5))
                .when(unavailable_row || current, |d| {
                    d.cursor_default().tab_stop(false)
                })
                .when(!unavailable_row && !current, |d| {
                    d.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.switch_channel(channel);
                        cx.notify();
                    }))
                }),
            );
        }
        list = list.child(
            option(
                "alt-build-keep",
                pinned.is_some(),
                tr("A specific version…").into(),
                match &pinned {
                    Some(tag) => build_name(tag),
                    None => tr("Pick one build and keep it until you change this.").into(),
                },
            )
            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                this.open_versions();
                cx.notify();
            })),
        );
        // Only the choices scroll when the window is short: the heading and an error stay in
        // view (#184).
        let choices = div()
            .id("alt-build-choices")
            .flex()
            .flex_col()
            .gap(px(10.))
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&crate::frame::scrolls("build-list"))
            .children(status.map(muted))
            .child(list);
        let body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(
                div()
                    .flex_none()
                    .text_size(px(tokens::CAPTION))
                    .text_color(rgb(palette().muted))
                    .child(tr("GAME BUILD")),
            )
            .child(choices)
            .children(self.ui.sheets.error.clone().map(|error| {
                build_menu_error(error, |whole| {
                    button("alt-build-error-details", tr("Details")).on_click(cx.listener(
                        move |this, _: &ClickEvent, _, cx| {
                            this.open_notice(whole.clone(), None);
                            cx.notify();
                        },
                    ))
                })
            }));
        // A popover under Home's build picker: the scrim is clear.
        let at = self.ui.home.chooser_at();
        self.popover(("alt-build", "alt-build-body"), body, at, cx)
    }

    /// The selected profile's options, under Home's menu button: Sunrise Extras (here only, no
    /// longer in the build chooser) and the profile's folder.
    fn profile_menu(&mut self, cx: &mut Context<Self>) -> Stateful<Div> {
        let Some(instance) = self.selected_instance() else {
            return div().id("alt-profile-menu");
        };
        let home = instance.home.clone();
        let folder = div()
            .id("alt-profile-folder")
            .reach()
            .flex()
            .items_center()
            .gap(px(12.))
            .p(px(10.))
            .rounded(crate::tokens::r(crate::tokens::R_CONTROL))
            .cursor_pointer()
            .hover(|d| d.bg(rgb(palette().hover)))
            .child(crate::glass_ui::icon(
                "folder-01",
                16.,
                rgb(palette().text).into(),
            ))
            .child(
                div()
                    .text_size(px(tokens::BODY))
                    .child(tr("Open profile folder")),
            )
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.close_sheet();
                crate::glass_toast::open(cx, &home);
                cx.notify();
            }));
        let body = div()
            .flex()
            .flex_col()
            .gap(px(10.))
            .child(extras().px(px(10.)))
            .child(div().h(px(1.)).bg(rgb(palette().line)))
            .child(folder);
        let at = self.ui.home.menu_at();
        self.popover(("alt-profile-menu", "alt-profile-menu-body"), body, at, cx)
    }

    /// A popover under one of Home's controls, its top left at `at`: a clear layer that a
    /// press outside closes, and `body` on a raised plate inside the window. `ids`: the
    /// layer's and the plate's.
    fn popover(
        &mut self,
        (layer, plate): (&'static str, &'static str),
        body: Div,
        (left, top): (f32, f32),
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let width = 372_f32.min(self.window_width - 24.);
        div()
            .id(layer)
            .absolute()
            .inset_0()
            .occlude()
            .on_mouse_down(MouseButton::Right, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
            .on_mouse_down(MouseButton::Middle, |_: &MouseDownEvent, _, cx| {
                cx.stop_propagation()
            })
            .on_scroll(|_, _, cx| cx.stop_propagation())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, _, cx| {
                    this.close_sheet();
                    cx.notify();
                }),
            )
            .child(
                body.id(plate)
                    .absolute()
                    // Never taller than the room under the picker: the choices scroll.
                    .max_h(px((self.window_height - top - 12.).max(120.)))
                    .overflow_hidden()
                    // Inside the window, however narrow it is.
                    .left(px(left.min(self.window_width - width - 12.).max(12.)))
                    .top(px(top))
                    .w(px(width))
                    .p(px(14.))
                    .rounded(crate::tokens::r(crate::tokens::R_CARD))
                    .border_1()
                    // The soft edge every sheet and picker has, not a light outline.
                    .border_color(rgb(palette().divider))
                    .bg(rgb(palette().raised))
                    .text_color(rgb(palette().text))
                    .on_mouse_down(MouseButton::Left, |_: &MouseDownEvent, _, cx| {
                        cx.stop_propagation()
                    }),
            )
    }
}

/// Sunrise Extras' switch, shown off and dim: it arrives with the build that supports it.
fn extras() -> Div {
    div()
        .flex()
        .items_start()
        .justify_between()
        .gap(px(12.))
        .opacity(0.5)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .child(div().text_size(px(tokens::BODY)).child(tr("Sunrise Extras")))
                .child(
                    div()
                        .text_size(px(tokens::SMALL))
                        .text_color(rgb(palette().muted))
                        .child(tr(
                            "Sunrise’s additions on top of vanilla. This switch arrives with the build that supports it.",
                        )),
                ),
        )
        .child(
            div()
                .mt(px(2.))
                .flex_none()
                .w(px(34.))
                .h(px(20.))
                .rounded_full()
                .border_1()
                .border_color(rgb(palette().line)),
        )
}

/// The pinned preview has room for two lines. Every error can open in the scrollable notice.
fn build_menu_error(error: String, details: impl FnOnce(String) -> Stateful<Div>) -> Div {
    div()
        .flex_none()
        .flex()
        .items_start()
        .gap(px(8.))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .line_clamp(2)
                .text_ellipsis()
                .text_color(rgb(palette().danger))
                .text_size(px(tokens::SMALL))
                .child(error.clone()),
        )
        .child(details(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_build_menu_marks_nightly_while_a_stable_profile_waits_for_stable() {
        use instances::Channel;
        let release = |tag: &str, nightly: bool| backend::Release {
            tag: tag.into(),
            name: String::new(),
            body: String::new(),
            date: String::new(),
            time: String::new(),
            published_at: String::new(),
            asset: Some(("sunrise.tar.gz".into(), 1)),
            prerelease: false,
            nightly: nightly.then(|| "nightly".into()),
        };
        let mut releases = vec![release("nightly-2026-11-20-aaaaaaa", true)];
        let marked = |releases: &[backend::Release]| {
            let waits = Channel::Stable.builds(releases).next().is_none();
            following_now(Channel::Stable, waits)
        };
        // The marked row is the channel whose build Play starts.
        let plays = Channel::Stable.play_target(&releases, &[], None, None);
        assert_eq!(marked(&releases), Channel::Nightly);
        assert_eq!(
            plays.and_then(|tag| Channel::of_build(tag)),
            Some(Channel::Nightly)
        );
        releases.push(release("26.3-0.1.0", false));
        let plays = Channel::Stable.play_target(&releases, &[], None, None);
        assert_eq!(marked(&releases), Channel::Stable);
        assert_eq!(
            plays.and_then(|tag| Channel::of_build(tag)),
            Some(Channel::Stable)
        );
        // Other channels are marked as saved.
        assert_eq!(following_now(Channel::Dev, false), Channel::Dev);
    }

    #[test]
    fn every_build_menu_error_offers_details_with_the_full_message() {
        let echoed = format!(
            "instances.json: invalid type: string \"{}\", expected u64 at line 1 column 412",
            "W".repeat(26)
        );
        assert_eq!(echoed.chars().count(), 100);
        for error in [
            echoed,
            "Wait for the current operation to finish.".to_owned(),
        ] {
            let mut opened = None;
            let _ = build_menu_error(error.clone(), |whole| {
                opened = Some(whole);
                button("alt-build-error-details", tr("Details"))
            });
            assert_eq!(opened.as_deref(), Some(error.as_str()));
        }
    }

    #[test]
    fn build_times_read_relative_for_a_week_then_as_the_date() {
        use chrono::TimeZone as _;
        let now = chrono::Utc
            .with_ymd_and_hms(2026, 11, 23, 16, 42, 0)
            .unwrap();
        let when = |date, time| built_when(date, time, now);
        assert_eq!(when("2026-11-23", "16:42").as_deref(), Some("just now"));
        assert_eq!(when("2026-11-23", "16:45").as_deref(), Some("just now"));
        assert_eq!(when("2026-11-23", "16:41").as_deref(), Some("1 minute ago"));
        assert_eq!(
            when("2026-11-23", "16:29").as_deref(),
            Some("13 minutes ago")
        );
        assert_eq!(when("2026-11-23", "15:30").as_deref(), Some("1 hour ago"));
        assert_eq!(when("2026-11-23", "01:00").as_deref(), Some("15 hours ago"));
        assert_eq!(when("2026-11-22", "10:00").as_deref(), Some("1 day ago"));
        assert_eq!(when("2026-11-21", "10:00").as_deref(), Some("2 days ago"));
        assert_eq!(when("2026-11-16", "16:43").as_deref(), Some("6 days ago"));
        assert_eq!(
            when("2026-11-16", "16:42").as_deref(),
            Some("16/11/2026 16:42 UTC")
        );
        // A tag's date alone counts whole days.
        assert_eq!(when("2026-11-23", "").as_deref(), Some("today"));
        assert_eq!(when("2026-11-22", "").as_deref(), Some("yesterday"));
        assert_eq!(when("2026-11-17", "").as_deref(), Some("6 days ago"));
        assert_eq!(when("2026-11-16", "").as_deref(), Some("16/11/2026"));
        assert_eq!(when("not a date", "16:42"), None);
    }

    #[test]
    fn build_labels_name_the_channel_and_date_or_version() {
        assert_eq!(build_name("nightly-2026-11-17-abcdef0"), "Nightly · Nov 17");
        assert_eq!(build_name("26.3-0.1.0"), "Stable · 0.1.0");
        assert_eq!(build_name("dev-2026-11-17-abcdef0"), "Dev · Nov 17");
        assert_eq!(build_name("v1.0.0"), "v1.0.0");
    }
}
