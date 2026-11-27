//! Mods › Installed: collection sections, one switch per row, and scoped bulk actions.
use super::model::{self, Row, Show};
use super::ui::*;
use super::*;
use crate::paint::palette;
use std::{collections::BTreeMap, rc::Rc};
use sunrise_launcher::browser::images;

/// Keep removal reviews on the same labels Installed paints, including hidden dependents.
pub(super) fn remove_names(rows: &[Row], labels: &[Option<String>]) -> BTreeMap<String, String> {
    rows.iter()
        .zip(labels)
        .filter(|(row, _)| row.selectable_archive())
        .map(|(row, label)| (row.id.clone(), label.as_ref().unwrap_or(&row.title).clone()))
        .collect()
}

impl ModsState {
    fn installed_filters(&mut self, rows: &[Row]) -> model::FilterCounts {
        let before = self.row_filter.clone();
        let counts = model::counted_filters(rows, &mut self.row_filter);
        if rows.is_empty() || self.row_filter != before {
            self.selection.clear();
            self.anchor = None;
        }
        counts
    }

    fn select_installed_row(&mut self, file: &str, shown: &[(String, String)], shift: bool) {
        let Some(pos) = shown.iter().position(|(path, _)| path == file) else {
            return;
        };
        let anchor = self
            .anchor
            .as_ref()
            .filter(|_| shift)
            .and_then(|anchor| shown.iter().position(|(path, _)| path == anchor));
        if let Some(anchor) = anchor {
            self.selection.extend(
                shown[anchor.min(pos)..=anchor.max(pos)]
                    .iter()
                    .map(|(_, id)| id.clone()),
            );
        } else {
            let id = &shown[pos].1;
            if !self.selection.remove(id) {
                self.selection.insert(id.clone());
            }
        }
        self.anchor = Some(file.to_owned());
    }
}

impl Launcher {
    pub(super) fn archive_display(&self, row: &Row) -> Option<&ArchiveDisplay> {
        self.ui
            .mods
            .snapshot
            .as_ref()?
            .archives
            .get(row.archive_file()?)
    }

    pub(super) fn installed(
        &mut self,
        window: &mut Window,
        focus: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let header = self.mods_header(true, focus, window, cx);
        let mut page = div()
            .size_full()
            .flex()
            .flex_col()
            .gap(px(14.))
            .p(px(24.))
            .text_color(c(palette().text))
            .child(header)
            .children(self.strip(cx));

        let refresh = match InstalledRefresh::of(self.ui.mods.snapshot.as_ref()) {
            InstalledRefresh::Ready => None,
            InstalledRefresh::Reading => Some(
                card()
                    .p(px(16.))
                    .child(muted(tr("Refreshing Installed mods…"))),
            ),
            InstalledRefresh::Failed(error, home) => {
                let (summary, action) = match error {
                    InstalledReadError::GameInstall(problem) => {
                        let home = home.to_owned();
                        let summary = problem.summary().to_owned();
                        let text = problem.details();
                        let action =
                            crate::frame::button("installed-recovery-details", tr("Show details"))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.open_notice(
                                        text.clone(),
                                        Some(crate::sheets::WayOut::ProfileFolder(home.clone())),
                                    );
                                    cx.notify();
                                }));
                        (summary, Some(action))
                    }
                    InstalledReadError::Other(error) => (
                        trf(
                            "Could not refresh Installed mods: {error}",
                            &[("error", error)],
                        ),
                        None,
                    ),
                };
                Some(
                    card()
                        .p(px(16.))
                        .flex()
                        .flex_col()
                        .items_start()
                        .gap(px(8.))
                        .child(muted(summary))
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .items_center()
                                .gap(px(8.))
                                .children(action)
                                .child(
                                    crate::frame::button("retry-installed", tr("Retry")).on_click(
                                        cx.listener(|this, _, _, cx| {
                                            if let Some(snapshot) = &mut this.ui.mods.snapshot {
                                                snapshot.key.clear();
                                            }
                                            this.mods_refresh();
                                            cx.notify();
                                        }),
                                    ),
                                ),
                        ),
                )
            }
        };
        let refresh_ready = refresh.is_none();
        page = page.children(refresh);
        let (rows, labels) = self.labeled_installed_rows();
        let (reading, failed) = match InstalledRefresh::of(self.ui.mods.snapshot.as_ref()) {
            InstalledRefresh::Ready => (false, false),
            InstalledRefresh::Reading => (true, false),
            InstalledRefresh::Failed(..) => (false, true),
        };
        self.ui.mods.drawn = Some(InstalledDrawn::of(reading, failed, rows.len()));
        if rows.is_empty() && !refresh_ready {
            return page.into_any_element();
        }
        let model::FilterCounts { shows, kinds } = self.ui.mods.installed_filters(&rows);
        if rows.is_empty() {
            return page
                .child(empty_state(
                    "puzzle",
                    tr("No mods yet"),
                    tr("Mods you add to this profile show up here."),
                    Some(
                        primary("alt-installed-browse", tr("Find mods")).on_click(cx.listener(
                            |this, _: &ClickEvent, _, cx| {
                                this.choose_view(Page::Discover);
                                cx.notify();
                            },
                        )),
                    ),
                ))
                .into_any_element();
        }

        let visible = model::visible(&rows, &self.ui.mods.row_filter);
        let shown: Rc<Vec<_>> = Rc::new(
            visible
                .iter()
                .filter_map(|&i| {
                    let row = &rows[i];
                    let file = row.archive_file().filter(|_| row.selectable_archive())?;
                    Some((file.to_owned(), row.id.clone()))
                })
                .collect(),
        );
        let filter = self.ui.mods.row_filter.clone();
        let selected: Vec<&Row> = rows
            .iter()
            .filter(|r| r.selectable_archive() && self.ui.mods.selection.contains(&r.id))
            .collect();

        // ---- what to show: the same row of chips Discover filters with
        let counted = |label: &str, n: usize| {
            trf("{label} · {n}", &[("label", label), ("n", &n.to_string())])
        };
        let mut chips = div().flex().flex_wrap().items_center().gap(px(8.));
        let show_options = shows
            .into_iter()
            .map(|(which, n)| {
                let label = match which {
                    Show::All => tr("All"),
                    Show::On => tr("Turned on"),
                    Show::Off => tr("Turned off"),
                };
                let pick: Choose = Rc::new(move |this, _| {
                    let m = &mut this.ui.mods;
                    m.row_filter.show = which;
                    m.selection.clear();
                    m.anchor = None;
                });
                (counted(label, n).into(), filter.show == which, pick)
            })
            .collect();
        chips = chips.child(choices("alt-show", show_options, cx).gap(px(8.)));
        if !kinds.is_empty() {
            chips = chips.child(div().flex_none().w(px(1.)).h(px(20.)).bg(c(palette().line)));
        }
        for (make, n) in kinds {
            let i = make as usize;
            chips = chips.child(
                // Two switches, not one choice: each is its own Tab stop.
                chip(
                    ("alt-kind", i),
                    counted(make.label(), n),
                    filter.makes.contains(&make),
                )
                .tab_index(0)
                .seen()
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let m = &mut this.ui.mods;
                    if !m.row_filter.makes.remove(&make) {
                        m.row_filter.makes.insert(make);
                    }
                    m.selection.clear();
                    m.anchor = None;
                    cx.notify();
                })),
            );
        }
        page = page.child(chips);

        // ---- toolbar: select-all and the bulk actions, naming their scope
        let selected_here = shown
            .iter()
            .filter(|(_, id)| self.ui.mods.selection.contains(id))
            .count();
        let all_on = selected_here > 0 && selected_here == shown.len();
        let n_sel = selected.len();
        let registry_selected = selected.iter().filter(|row| row.local.is_none()).count();
        let busy = !refresh_ready || self.mods_busy();
        let with_update = selected.iter().filter(|r| model::bulk_updatable(r)).count();
        let registry_updates = rows.iter().filter(|r| model::bulk_updatable(r)).count();
        let has_manual_mods = rows.iter().any(|r| r.manual_badge().is_some());
        let source_update = self.nightly_catalogue_current()
            && rows
                .iter()
                .any(|row| nightly::installed_source(row).is_some() && row.update.is_some());
        let has_tracked_sources = rows
            .iter()
            .filter_map(nightly::installed_source)
            .any(|origin| {
                origin.source.as_ref().is_some_and(|source| {
                    !matches!(
                        source.track,
                        sunrise_launcher::github_mods::SourceTrack::Pinned
                    )
                })
            });
        let bulk = |id: &'static str, label: String, enabled: bool, danger: bool| {
            let b = crate::frame::button(id, label);
            let b = if danger {
                b.text_color(c(palette().danger))
            } else {
                b
            };
            if enabled && !busy {
                b
            } else {
                dim(b)
            }
        };
        let toolbar = {
            let all_shown = shown.clone();
            let select_all = checkbox("alt-select-all", all_on)
                .when(selected_here > 0 && !all_on, |d| {
                    d.bg(c(palette().text))
                        .border_color(c(palette().text))
                        .child(div().w(px(10.)).h(px(2.)).bg(c(palette().background)))
                })
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    let m = &mut this.ui.mods;
                    match all_shown.iter().all(|(_, id)| m.selection.contains(id)) {
                        true => m.selection.clear(),
                        false => m
                            .selection
                            .extend(all_shown.iter().map(|(_, id)| id.clone())),
                    }
                    m.anchor = None;
                    cx.notify();
                }));
            let mut bar = div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(14.))
                .min_h(px(48.))
                .when(n_sel > 0 || has_tracked_sources || has_manual_mods, |bar| {
                    bar.flex_wrap().py(px(7.)).gap_y(px(6.))
                })
                .border_b_1()
                .border_color(c(palette().divider))
                .child(select_all);
            if n_sel == 0 {
                bar = bar
                    .child(muted(model::visible_count(&rows, &visible)))
                    .child(div().flex_1());
                if has_manual_mods {
                    bar = bar.child(muted(tr(
                        "From source and Local mods refresh only via Add from source",
                    )));
                }
                if has_tracked_sources {
                    let checking = self.source_updates_checking();
                    let check = bulk(
                        "nightly-check-source-updates",
                        if checking {
                            tr("Checking source updates…")
                        } else {
                            tr("Check for source updates")
                        }
                        .into(),
                        !checking,
                        false,
                    );
                    bar = bar.child(if checking || busy {
                        check
                    } else {
                        check.on_click(cx.listener(|this, _, _, cx| {
                            this.check_source_updates(None);
                            cx.notify();
                        }))
                    });
                }
                if registry_updates > 0 || source_update {
                    let label = if registry_updates > 0 && !source_update {
                        trf(
                            "Update all ({count})",
                            &[("count", &registry_updates.to_string())],
                        )
                    } else {
                        tr("Update all").into()
                    };
                    let update_all = bulk("alt-update-all", label, true, false);
                    bar = bar.child(if busy {
                        update_all
                    } else {
                        let names = remove_names(&rows, &labels);
                        update_all.on_click(cx.listener(move |this, _, _, cx| {
                            this.submit_mod_update_all(names.clone());
                            cx.notify();
                        }))
                    });
                }
            } else {
                let on = bulk(
                    "alt-bulk-on",
                    trf(
                        "Turn on {count}",
                        &[("count", &registry_selected.to_string())],
                    ),
                    registry_selected > 0,
                    false,
                );
                let off = bulk(
                    "alt-bulk-off",
                    trf(
                        "Turn off {count}",
                        &[("count", &registry_selected.to_string())],
                    ),
                    registry_selected > 0,
                    false,
                );
                let upd = bulk(
                    "alt-bulk-update",
                    trf("Update {count}", &[("count", &with_update.to_string())]),
                    with_update > 0,
                    false,
                );
                let rm = bulk(
                    "alt-bulk-remove",
                    trf("Remove {count}…", &[("count", &n_sel.to_string())]),
                    true,
                    true,
                )
                .whitespace_nowrap();
                let remove = if busy {
                    rm
                } else {
                    let names = remove_names(&rows, &labels);
                    let chosen: Vec<_> = selected.iter().map(|row| row.id.clone()).collect();
                    let home = self.mods_home();
                    rm.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.ask_remove(&home, chosen.clone(), names.clone());
                        cx.notify();
                    }))
                };
                bar =
                    bar.child(div().text_size(px(tokens::LABEL)).child(
                        crate::selectable::selectable(trf(
                            "{count} selected of {shown} shown",
                            &[
                                ("count", &n_sel.to_string()),
                                ("shown", &shown.len().to_string()),
                            ],
                        )),
                    ))
                    .child(if !busy && registry_selected > 0 {
                        on.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.bulk_on();
                            cx.notify();
                        }))
                    } else {
                        on
                    })
                    .child(if !busy && registry_selected > 0 {
                        off.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.bulk_off();
                            cx.notify();
                        }))
                    } else {
                        off
                    })
                    .child(if !busy && with_update > 0 {
                        upd.on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.ask_update(true);
                            cx.notify();
                        }))
                    } else {
                        upd
                    })
                    .child(remove)
                    .child(div().flex_1())
                    .child(
                        muted(tr("Clear"))
                            .id("alt-selection-clear")
                            .reach()
                            .cursor_pointer()
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.ui.mods.selection.clear();
                                this.ui.mods.anchor = None;
                                cx.notify();
                            })),
                    );
            }
            bar
        };

        let mut list = div().flex().flex_col().children(
            self.source_update_notice(None)
                .map(|notice| note(notice.to_owned()).p(px(10.))),
        );
        // Collection headings, gaps and rows in one flat list, so only those near the
        // screen are built: a big pack has hundreds of rows, each with its own controls.
        let mut items = Vec::new();
        let mut n = 0;
        for (collection, group) in model::installed_groups(&rows, &visible) {
            if let Some(title) = collection {
                items.push(InstalledItem::Head(title, group.len()));
            } else if n != 0 {
                // A gap ends the previous collection without giving loose mods a heading.
                items.push(InstalledItem::Gap);
            }
            for &i in group {
                items.push(InstalledItem::Row(i, n));
                n += 1;
            }
        }
        let cull = self.mods.installed_list.clone();
        // Measurements belong to the items they were taken of, including local archive paths
        // when two archives share a mod ID.
        cull.items(installed_items_key(&items, &rows));
        // The row another page opened Installed at stays built until it has been scrolled to.
        let reveal = match &self.ui.mods.reveal {
            Some(search::Reveal::Mod(id)) if !self.ui.mods.revealed.get() => items
                .iter()
                .position(|item| matches!(item, InstalledItem::Row(i, _) if rows[*i].id == *id)),
            _ => None,
        };
        let scroll = crate::frame::scrolls("installed");
        list = list.child(cull.column(
            &scroll,
            items.len(),
            0.,
            |k| Some(k) == reveal,
            |k| {
                match items[k] {
                    InstalledItem::Head(title, count) => div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .px(px(14.))
                        .min_h(px(tokens::FIELD))
                        .bg(c(palette().background))
                        .child(
                            div()
                                .text_size(px(tokens::CAPTION))
                                .text_color(c(palette().muted))
                                .child(crate::selectable::selectable(trf(
                                    "{title} · {count}",
                                    &[("title", title), ("count", &count.to_string())],
                                ))),
                        )
                        .into_any_element(),
                    InstalledItem::Gap => div()
                        .h(px(12.))
                        .bg(c(palette().background))
                        .into_any_element(),
                    InstalledItem::Row(i, n) => self
                        .row(
                            n,
                            rows[i].clone(),
                            labels[i].clone(),
                            shown.clone(),
                            busy,
                            cx,
                        )
                        .into_any_element(),
                }
            },
        ));
        if visible.is_empty() && refresh_ready {
            list = list.child(
                div()
                    .p(px(16.))
                    .child(muted(tr("No mods match this search or filter."))),
            );
        }
        let hidden = self
            .ui
            .mods
            .snapshot
            .as_ref()
            .map_or(0, |s| s.entries.len().saturating_sub(rows.len()));
        let right = card()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(toolbar)
            .child(crate::scrollbar::wrap_page_in_margin(
                "installed",
                div()
                    .id("alt-installed-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
                &self.theme,
            ))
            .children((hidden > 0).then(|| {
                div().px(px(14.)).py(px(8.)).border_t_1().border_color(c(palette().divider)).child(muted(trf(
                    "{count} other files the launcher installed (packs, or from other sources) aren't listed here.",
                    &[("count", &hidden.to_string())],
                )))
            }));
        page = page.child(right);
        page.into_any_element()
    }

    pub(super) fn labeled_installed_rows(&self) -> (Vec<Row>, Vec<Option<String>>) {
        let mut rows = self.rows();
        // Use the exact installed package's display name here, as in the Remove review.
        // Compute collisions before filtering so a row keeps its label when peers are hidden.
        for row in &mut rows {
            if let Some(archive) = self.archive_display(row) {
                row.title.clone_from(&archive.name);
                if row.authors.is_empty() {
                    row.authors = archive.authors.join(", ");
                }
            }
            if let Some(origin) = nightly::installed_source(row) {
                let update = self.source_update_version(origin).map(str::to_owned);
                if let Some(source) = self.source_listing(origin) {
                    row.authors = source.owner().to_owned();
                    row.collection = self.source_collection_for(source).map(Into::into);
                    row.source_listing = Some(source.clone());
                }
                row.update = update;
            }
            if let Some(origin) = row.local.as_ref().and_then(|local| local.github.as_ref()) {
                if row.authors.is_empty() && !origin.owner.is_empty() {
                    row.authors.clone_from(&origin.owner);
                }
            }
        }
        model::group_installed(&mut rows);
        use installed::{ModLabel, ModLabelSource};
        let labels = installed::colliding_mod_labels(rows.iter().map(|row| {
            let (authors, version) = self.archive_display(row).map_or_else(
                || (std::slice::from_ref(&row.authors), row.version.as_str()),
                |archive| (archive.authors.as_slice(), archive.version.as_str()),
            );
            let source = match &row.local {
                None => ModLabelSource::Registry,
                Some(local) if local.github.is_some() => ModLabelSource::Github,
                Some(_) => ModLabelSource::Local,
            };
            ModLabel {
                id: &row.id,
                name: &row.title,
                authors,
                version,
                source,
            }
        }));
        (rows, labels)
    }

    fn row(
        &mut self,
        n: usize,
        r: Row,
        label: Option<String>,
        shown: Rc<Vec<(String, String)>>,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        let selected = self.ui.mods.selection.contains(&r.id);
        let package = self
            .archive_display(&r)
            .and_then(|archive| archive.icon.clone());
        let artwork = if let Some(source) = &r.source_listing {
            self.source_tile(
                source,
                None,
                self.nightly_catalogue_current(),
                package,
                32.,
                cx,
            )
        } else {
            let listed = r
                .icon
                .as_ref()
                .and_then(|url| self.mods_image(url, images::ROW));
            tile(mod_artwork(listed, package), &r.id, &r.title, 32.)
        };
        let error = self.ui.mods.errors.get(&r.id).cloned();

        let check = {
            let file = r.archive_file().map(str::to_owned);
            checkbox(("alt-row-check", n), selected).on_click(cx.listener(
                move |this, ev: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    if let Some(file) = &file {
                        this.ui
                            .mods
                            .select_installed_row(file, &shown, ev.modifiers().shift);
                    }
                    cx.notify();
                },
            ))
        };

        // The list's shared columns (`model::cells`) keep their places across every group.
        // Narrow, the version starts under the name, then kind and status; the action and
        // switch stay at the right.
        let wide = self.content_width() >= 860.;
        use super::build_run::State;
        let building = self
            .build_here()
            .filter(|_| r.on)
            .and_then(|run| run.state_of(&r.id, &r.version));
        let failed = matches!(building, Some(State::Failed(_)));
        let cells = model::cells(&r, error.as_deref(), building);
        let tone = |tone: model::Tone| match tone {
            model::Tone::Warning => palette().warning,
            model::Tone::Info => palette().info,
            model::Tone::Accent => palette().accent_ink,
            model::Tone::Success => palette().success,
            model::Tone::Muted => palette().muted,
        };
        let install_hold = r
            .entry
            .as_ref()
            .and_then(|entry| entry.install_hold.as_ref())
            .filter(|_| !r.on);
        let hold = self
            .ui
            .mods
            .snapshot
            .as_ref()
            .and_then(|snapshot| snapshot.rows.switch_held.get(&r.id));
        let hold_label = hold.map(|hold| (hold.label(), hold.detail().map(str::to_owned)));
        let switch_held = hold_label.is_some();
        let status_cell: AnyElement = if switch_held {
            div().into_any_element()
        } else if let Some(hold) = install_hold {
            let detail = hold.detail();
            status("warning", hold.label(), palette().warning)
                .id(("alt-row-install-hold", n))
                .reach()
                .min_w_0()
                .cursor_pointer()
                .tooltip(crate::glass_ui::tip(tr("Show why")))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    this.open_notice(detail.clone(), None);
                    cx.stop_propagation();
                    cx.notify();
                }))
                .into_any_element()
        } else {
            match (&cells.status, r.unfit.clone()) {
                (Some(_), Some(unfit)) if cells.popover => {
                    self.unfit(n, &r, unfit, busy, cx).into_any_element()
                }
                // A mod the last build could not build: its status opens that build's window
                // again, where Details and the build log are.
                (Some(said), _) if failed && said.label == tr("Failed") => {
                    status(said.icon, said.label.clone(), tone(said.tone))
                        .id(("alt-row-failed", n))
                        .reach()
                        .min_w_0()
                        .cursor_pointer()
                        .hover(|s| s.text_color(c(palette().text)))
                        .tooltip(crate::glass_ui::tip(said.tip.clone()))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.open_sheet(crate::sheets::Sheet::ModBuild);
                            cx.stop_propagation();
                            cx.notify();
                        }))
                        .into_any_element()
                }
                (Some(said), _) => status(said.icon, said.label.clone(), tone(said.tone))
                    .id(("alt-row-status", n))
                    .min_w_0()
                    .tooltip(crate::glass_ui::tip(said.tip.clone()))
                    .into_any_element(),
                (None, _) => div().into_any_element(),
            }
        };
        let action = cells.action.map(|action| {
            let (id, label) = match action {
                model::RowAction::Update => ("alt-row-update", tr("Update")),
            };
            let b = crate::frame::button((id, n), label);
            let stale_source = action == model::RowAction::Update
                && nightly::installed_source(&r).is_some()
                && !self.nightly_catalogue_current();
            if busy || stale_source {
                return dim(b).on_click(|_, _, cx| cx.stop_propagation());
            }
            let row = r.clone();
            b.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                match action {
                    model::RowAction::Update if nightly::installed_source(&row).is_some() => {
                        this.submit_source_update(&row.id);
                    }
                    model::RowAction::Update => this.update_one(&row),
                }
                cx.notify();
            }))
        });
        let sw = {
            let s = ui::switch(("alt-row-switch", n), r.on).when(cells.unavailable, |s| {
                s.opacity(0.52).tooltip(crate::glass_ui::tip(tr(
                    "Enabled for this profile, but not included in the build.",
                )))
            });
            if cells.switch && !busy {
                let row = r.clone();
                s.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.toggle(&row);
                    cx.notify();
                }))
            } else {
                s.opacity(0.45)
                    .cursor_default()
                    .tab_stop(false)
                    .on_click(|_, _, cx| cx.stop_propagation())
            }
        };
        let manual_badge = r.manual_badge();
        let no_auto_updates = r.is_manual() && action.is_none();
        let source = r.source_label();
        let byline = if label.is_some() || r.authors.is_empty() {
            source.to_owned()
        } else {
            trf(
                "{author} · {source}",
                &[("author", &r.authors), ("source", source)],
            )
        };
        let title = div()
            .flex()
            .flex_col()
            .min_w_0()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .min_w_0()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .truncate()
                            .child(label.unwrap_or_else(|| r.title.clone())),
                    )
                    .children(manual_badge.map(ui::manual_badge_pill)),
            )
            .child(
                div()
                    .id(("installed-origin", n))
                    .text_size(px(tokens::SMALL))
                    .text_color(c(palette().muted))
                    .truncate()
                    .tooltip(crate::glass_ui::tip(byline.clone()))
                    .child(byline),
            )
            .children(
                nightly::installed_source(&r)
                    .and(r.update.as_ref())
                    .map(|version| {
                        muted(trf("Update available: {version}", &[("version", version)]))
                    }),
            )
            .children(hold_label.map(|(label, detail)| {
                div()
                    .id(("alt-row-hold", n))
                    .text_size(px(tokens::SMALL))
                    .text_color(c(palette().warning))
                    .truncate()
                    .when_some(detail, |hold, detail| {
                        hold.tooltip(crate::glass_ui::tip(detail))
                    })
                    .child(label)
            }))
            .children(
                nightly::installed_source(&r)
                    .and_then(|_| {
                        self.source_outcome(&r.id)
                            .and_then(|result| result.as_ref().err())
                            .or_else(|| self.source_update_issue(&r.id))
                            .and_then(|error| nightly::source_error(error, None))
                    })
                    .map(|line| {
                        div()
                            .id(("installed-source-issue", n))
                            .text_color(c(palette().warning))
                            .text_size(px(tokens::SMALL))
                            .truncate()
                            .tooltip(crate::glass_ui::tip(line.clone()))
                            .child(line)
                    }),
            )
            .when_some(r.registry_issue.clone(), |d, issue| {
                d.child(
                    div()
                        .id(("installed-registry-issue", n))
                        .text_size(px(tokens::SMALL))
                        .text_color(c(palette().muted))
                        .truncate()
                        .tooltip(crate::glass_ui::tip(issue.clone()))
                        .child(issue),
                )
            });
        use model::column;
        let kind = |width: f32| {
            div()
                .w(px(width))
                .flex_none()
                .text_size(px(tokens::LABEL))
                .text_color(c(palette().muted))
                .child(cells.kind)
        };
        let version = |width: f32| {
            div()
                .w(px(width))
                .flex_none()
                .truncate()
                .text_size(px(tokens::LABEL))
                .when(wide, |d| d.font_family(crate::theme::MONO))
                .child(cells.version.clone())
        };
        let row = div()
            .id(("alt-row", n))
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(14.))
            .min_h(px(52.))
            .border_t_1()
            .border_color(c(palette().divider))
            .when(selected, |d| d.bg(c(palette().selected)))
            // The mod another page opened Installed at.
            .when(
                self.ui.mods.reveal == Some(search::Reveal::Mod(r.id.clone())),
                |d| {
                    d.bg(c(palette().hover)).child(super::ui::reveal_in(
                        crate::frame::scrolls("installed"),
                        self.ui.mods.revealed.clone(),
                        None,
                        Some(self.mods.installed_list.clone()),
                    ))
                },
            )
            .child(if r.selectable_archive() {
                div().flex_none().child(check)
            } else {
                div().flex_none().w(px(18.))
            })
            .child(artwork);
        let row = if wide {
            row.child(title.flex_1().min_w(px(140.)))
                .child(kind(column::KIND))
                .child(version(column::VERSION))
                .child(
                    div()
                        .w(px(column::STATUS))
                        .flex_none()
                        .flex()
                        .items_center()
                        .child(status_cell),
                )
                .child(
                    div()
                        .w(px(column::ACTION))
                        .flex_none()
                        .flex()
                        .justify_end()
                        .children(action)
                        .when(no_auto_updates, |d| d.child(muted(tr("No auto-updates")))),
                )
        } else {
            row.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .py(px(8.))
                    .flex()
                    .flex_col()
                    .gap(px(2.))
                    .child(title)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .child(version(60.))
                            .child(kind(column::KIND))
                            .child(div().flex_1().min_w_0().flex().child(status_cell)),
                    ),
            )
            .children(action)
            .when(no_auto_updates, |d| d.child(muted(tr("No auto-updates"))))
        };
        row.reach()
            .cursor_pointer()
            .hover(|s| s.bg(c(palette().hover)))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.mods_show_installed_project(&r);
                cx.notify();
            }))
            .child(
                div()
                    .w(px(column::SWITCH))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .child(sw),
            )
    }

    /// Where a row's switch would be, for a mod the game was made without: "Not on Nov 17" and
    /// a "?" that says why in one sentence, the changed items for its author, and the way on.
    pub(super) fn unfit(
        &self,
        n: usize,
        r: &Row,
        unfit: model::Unfit,
        busy: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        use crate::frame::button;
        let open = self.ui.mods.unfit_open.as_deref() == Some(r.id.as_str());
        let id = r.id.clone();
        let help = status(
            "information-circle",
            tr("Needs an update"),
            palette().warning,
        )
        .id(("alt-unfit-help", n))
        .reach()
        // Its glyph stands where every other row's does.
        .px(px(8.))
        .ml(px(-8.))
        .h(px(28.))
        .when(open, |d| d.bg(c(palette().hover)))
        .hover(|s| s.bg(c(palette().hover)))
        .cursor_pointer()
        .tooltip(crate::glass_ui::tip(trf(
            "{mod} needs an update for {build}. It comes back when its author releases one.",
            &[("mod", &r.title), ("build", &unfit.on)],
        )))
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            let m = &mut this.ui.mods;
            m.unfit_open = match m.unfit_open.as_ref() == Some(&id) {
                true => None,
                false => Some(id.clone()),
            };
            m.unfit_details = false;
            cx.stop_propagation();
            cx.notify();
        }));
        let said = open.then(|| {
            let sentence = match &unfit.made_for {
                Some((_, made)) => trf(
                    "{mod} needs an update for {on}. It was made for {made}, and comes back when its author releases a new version. It stays installed.",
                    &[("mod", &r.title), ("on", &unfit.on), ("made", made)],
                ),
                None => trf(
                    "{mod} needs an update for {on}. It comes back when its author releases a new version. It stays installed.",
                    &[("mod", &r.title), ("on", &unfit.on)],
                ),
            };
            let details = self.ui.mods.unfit_details;
            let technical = (!unfit.changes.is_empty()).then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.))
                    .child(
                        div()
                            .id("alt-unfit-details")
.reach()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .text_size(px(tokens::SMALL))
                            .text_color(c(palette().muted))
                            .cursor_pointer()
                            .child(crate::glass_ui::disclosure_chevron(
                                "installed-unfit-details",
                                details,
                                12.,
                                c(palette().muted),
                            ))
                            .child(tr("Details"))
                            .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                                this.ui.mods.unfit_details = !this.ui.mods.unfit_details;
                                cx.stop_propagation();
                                cx.notify();
                            })),
                    )
                    .children(details.then(|| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.))
                            .children(unfit.changes.iter().map(|change| {
                                div()
                                    .text_size(px(tokens::SMALL))
                                    .font_family(crate::theme::MONO)
                                    .text_color(c(palette().muted))
                                    .child(change.clone())
                            }))
                    }))
            });
            // One way on: a version that fits when the registry has a newer one, else back to
            // the build it was made for when that build is still here.
            let action = match (busy, &r.update, &unfit.made_for) {
                (true, _, _) => None,
                (false, Some(_), _) => {
                    let b = button(
                        "alt-unfit-update",
                        trf("Update {mod}", &[("mod", &r.title)]),
                    );
                    if nightly::installed_source(r).is_some() && !self.nightly_catalogue_current() {
                        Some(dim(b).on_click(|_, _, cx| cx.stop_propagation()))
                    } else {
                        let row = r.clone();
                        Some(b.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            this.ui.mods.unfit_open = None;
                            if nightly::installed_source(&row).is_some() {
                                this.submit_source_update(&row.id);
                            } else {
                                this.update_one(&row);
                            }
                            cx.stop_propagation();
                            cx.notify();
                        })))
                    }
                }
                (false, None, Some((tag, made))) if self.installed.contains(tag) => {
                    let b = button(
                        "alt-unfit-stay",
                        trf("Stay on {build}", &[("build", made)]),
                    );
                    let tag = tag.clone();
                    Some(b.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        this.ui.mods.unfit_open = None;
                        this.stay_on(tag.clone());
                        cx.stop_propagation();
                        cx.notify();
                    })))
                }
                _ => {
                    let id = r.id.clone();
                    let origin = nightly::installed_source(r).cloned();
                    Some(button("alt-unfit-check", tr("Check for update")).on_click(
                        cx.listener(move |this, _: &ClickEvent, _, cx| {
                            if let Some(origin) = &origin {
                                this.check_source_updates(Some(origin));
                            } else {
                                this.check_mod_update(&id);
                            }
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ))
                }
            };
            // Opens under the "?", or over it when the row is low in the window.
            let popup = div()
                    .w(px(360.))
                    .max_w(px(self.content_width() - 28.))
                    .p(px(14.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .rounded(crate::tokens::r(crate::tokens::R_CONTROL))
                    .border_1()
                    .border_color(palette().input_edge())
                    .bg(c(palette().raised))
                    .shadow_lg()
                    .occlude()
                    .child(div().text_size(px(tokens::LABEL)).child(sentence))
                    .children(technical)
                    .children(action.map(|a| div().flex().justify_end().child(a)));
            gpui::deferred(
                div().absolute().top(px(24.)).right_0().child(
                    gpui::anchored()
                        .anchor(gpui::Anchor::TopRight)
                        .snap_to_window_with_margin(px(8.))
                        .child(popup),
                ),
            )
            .with_priority(10)
        });
        div()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .child(div().relative().flex_none().child(help).children(said))
    }

    /// "Stay on Nov 16" for a mod made for it: the setup stays on that build, where the mod
    /// runs. The next Play builds there as usual.
    fn stay_on(&mut self, tag: String) {
        let Some(id) = self.selected_instance().map(|i| i.id.clone()) else {
            return;
        };
        if !self.guard() {
            return;
        }
        match instances::edit(&id, |meta| meta.version = Some(tag)) {
            Ok(_) => self.reload_instances(),
            Err(e) => self.ui.mods.notice = Some(Err(e)),
        }
    }
}

enum InstalledItem<'a> {
    Head(&'a str, usize),
    Gap,
    /// Index into `rows`, and the row's place among the rows shown.
    Row(usize, usize),
}

fn installed_items_key(items: &[InstalledItem<'_>], rows: &[Row]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    for item in items {
        match item {
            InstalledItem::Head(title, _) => (0u8, *title).hash(&mut hash),
            InstalledItem::Gap => 1u8.hash(&mut hash),
            InstalledItem::Row(i, _) => {
                (2u8, rows[*i].id.as_str(), rows[*i].archive_file()).hash(&mut hash)
            }
        }
    }
    hash.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `test` again in a child process with scratch homes, as the other window tests do:
    /// `None` here, the scratch root in the child.
    fn scratch_child(test: &str) -> Option<std::path::PathBuf> {
        const CHILD: &str = "SUNRISE_INSTALLED_VIEW_TEST";
        if let Some(root) = std::env::var_os(CHILD) {
            return Some(root.into());
        }
        let scratch = tempfile::tempdir().unwrap();
        let root = scratch.path();
        let nowhere = "http://127.0.0.1:9";
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &format!("mods::installed_view::tests::{test}")])
            .env(CHILD, root)
            .env("SUNRISE_HOME", root.join("game"))
            .env("SUNRISE_LAUNCHER_HOME", root.join("launcher"))
            .env("SUNRISE_LAUNCHER_LOCATION_FILE", root.join("location.json"))
            .env("SUNRISE_NO_KEYCHAIN", "1")
            .env("SUNRISE_LAUNCHER_NO_SELF_UPDATE", "1")
            .env("SUNRISE_TELEMETRY_URL", "off")
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("HTTP_PROXY", nowhere)
            .env("HTTPS_PROXY", nowhere)
            .env("ALL_PROXY", nowhere)
            .env_remove("NO_PROXY")
            .env_remove("SUNRISE_GITHUB_MIRROR")
            .env_remove("SUNRISE_LAUNCHER_BENCH")
            .status()
            .unwrap();
        assert!(status.success(), "{test} failed in its child process");
        None
    }

    #[test]
    fn a_mod_rewritten_in_place_shows_on_an_idle_page_once_due() {
        if scratch_child("a_mod_rewritten_in_place_shows_on_an_idle_page_once_due").is_none() {
            return;
        }
        let profile = sunrise_launcher::instances::create("Pack", None).unwrap();
        std::fs::create_dir_all(profile.home.join("sunrise-mods")).unwrap();
        let entries: Vec<_> = (0..3)
            .map(|n| {
                let id = format!("mod-{n}");
                let file = format!("sunrise-mods/{id}.smod");
                std::fs::write(profile.home.join(&file), b"one").unwrap();
                serde_json::json!({
                    "source": "sunrise", "project": id, "slug": id, "title": format!("Mod {n}"),
                    "kind": "mod", "version": "1.0.0", "number": "1.0.0", "file": file,
                    "checksum": format!("SHA-256:{}", "0".repeat(64)), "page": ""
                })
            })
            .collect();
        std::fs::write(
            profile.home.join("mod-browser.json"),
            serde_json::json!({ "entries": entries }).to_string(),
        )
        .unwrap();
        let mut settings = sunrise_launcher::backend::Settings::load();
        settings.instance = Some(profile.id.clone());
        settings.save().unwrap();

        let mut app = gpui::TestAppContext::single();
        let (shell, cx) = app.add_window_view(crate::window_root::Shell::new_for_test);
        let launcher = shell.read_with(cx, |shell, _| shell.launcher());
        launcher.update(cx, |l, cx| {
            l.show_installed_mods();
            cx.notify();
        });
        let frame = |cx: &mut gpui::VisualTestContext| {
            launcher.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        };
        let read = |cx: &mut gpui::VisualTestContext| {
            launcher.read_with(cx, |l, _| {
                l.ui.mods
                    .snapshot
                    .as_ref()
                    .filter(|s| !s.reading)
                    .map(|s| s.key.clone())
            })
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let wait = |cx: &mut gpui::VisualTestContext, not: Option<&str>| loop {
            assert!(
                std::time::Instant::now() < deadline,
                "the mods were never read"
            );
            frame(cx);
            if let Some(key) = read(cx).filter(|key| Some(key.as_str()) != not) {
                return key;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let first = wait(cx, None);

        // Rewritten where it lies: the next frame keeps the fingerprint it has.
        std::fs::write(profile.home.join("sunrise-mods/mod-1.smod"), b"one, longer").unwrap();
        frame(cx);
        assert_eq!(read(cx).as_deref(), Some(first.as_str()));
        // Before it is due the pump sees nothing to do; once due it sees the change, with no
        // input in between, and the next frame reads the mods again.
        let now = std::time::Instant::now();
        assert!(!launcher.update(cx, |l, _| l.mods_check_due(now)));
        let due = now + sunrise_launcher::native_status::FULL_EVERY;
        assert!(launcher.update(cx, |l, _| l.mods_check_due(due)));
        let second = wait(cx, Some(&first));
        assert_ne!(second, first);
        // An unchanged folder asks for nothing, even when due.
        let again = due + sunrise_launcher::native_status::FULL_EVERY;
        assert!(!launcher.update(cx, |l, _| l.mods_check_due(again)));
    }

    #[test]
    fn a_mod_far_down_the_list_is_built_and_revealed() {
        if scratch_child("a_mod_far_down_the_list_is_built_and_revealed").is_none() {
            return;
        }
        // A profile with 300 mods, far more than a screen.
        let profile = sunrise_launcher::instances::create("Pack", None).unwrap();
        std::fs::create_dir_all(profile.home.join("sunrise-mods")).unwrap();
        let entries: Vec<_> = (0..300)
            .map(|n| {
                let id = format!("mod-{n:03}");
                let file = format!("sunrise-mods/{id}.smod");
                std::fs::write(profile.home.join(&file), b"").unwrap();
                serde_json::json!({
                    "source": "sunrise", "project": id, "slug": id, "title": format!("Mod {n:03}"),
                    "kind": "mod", "version": "1.0.0", "number": "1.0.0", "file": file,
                    "checksum": format!("SHA-256:{}", "0".repeat(64)), "page": ""
                })
            })
            .collect();
        std::fs::write(
            profile.home.join("mod-browser.json"),
            serde_json::json!({ "entries": entries }).to_string(),
        )
        .unwrap();
        let mut settings = sunrise_launcher::backend::Settings::load();
        settings.instance = Some(profile.id.clone());
        settings.save().unwrap();

        let mut app = gpui::TestAppContext::single();
        let (shell, cx) = app.add_window_view(crate::window_root::Shell::new_for_test);
        let launcher = shell.read_with(cx, |shell, _| shell.launcher());
        // Opened at the 251st mod, as Home's mod count opens it at one.
        launcher.update(cx, |l, cx| {
            l.open_installed(super::super::search::Installed::Mod("mod-250".into()));
            cx.notify();
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let frame = |cx: &mut gpui::VisualTestContext| {
            launcher.update(cx, |_, cx| cx.notify());
            cx.run_until_parked();
        };
        while !launcher.read_with(cx, |l, _| l.ui.mods.revealed.get()) {
            assert!(
                std::time::Instant::now() < deadline,
                "the mod was never scrolled to"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
            frame(cx);
        }
        // Draw a few more frames so any remaining layout adjustments settle.
        for _ in 0..4 {
            frame(cx);
        }
        let scroll = cx
            .update(|_, _| crate::frame::drawn_scroll("installed"))
            .unwrap();
        let view = scroll.bounds();
        assert!(view.size.height > px(0.));
        let cull = launcher.read_with(cx, |l, _| l.mods.installed_list.clone());
        assert!(cull.settled(), "the list did not settle after reveal");
        let row = cull
            .drawn_bounds(250)
            .expect("mod-250 was not built after settling");
        assert!(
            row.top() >= view.top() && row.bottom() <= view.bottom(),
            "mod-250 ({row:?}) is outside the viewport ({view:?})"
        );
    }

    #[test]
    fn shift_click_resolves_its_archive_after_a_catalogue_reply_regroups_the_list() {
        let mut mods = ModsState::default();
        let local = ["ant", "bird", "cat", "dog"].map(|id| model::Local {
            id: id.into(),
            file: format!("sunrise-native-mods/{id}.smod"),
            name: id.into(),
            version: "1.0.0".into(),
            github: None,
            native: true,
            served_by: None,
            unreadable: false,
        });
        let mut rows = model::rows(
            &[],
            &model::World {
                v2: None,
                v2_failure: None,
                build: None,
                unfit_updates: &BTreeSet::new(),
                enabled: &HashSet::new(),
                unbuilt: &HashSet::new(),
                chosen: &BTreeMap::new(),
                local: &local,
            },
        );
        let shown = |rows: &[Row]| -> Vec<_> {
            rows.iter()
                .map(|row| (row.archive_file().unwrap().to_owned(), row.id.clone()))
                .collect()
        };
        let before = shown(&rows);
        mods.select_installed_row(&local[2].file, &before, false);
        // The catalogue lists Cat in a collection after the first click.
        rows[2].collection = Some(model::CollectionGroup {
            id: "pets".into(),
            name: "Pets".into(),
        });
        model::group_installed(&mut rows);
        let after = shown(&rows);
        assert_eq!(after[0].0, local[2].file);
        mods.select_installed_row(&local[0].file, &after, true);
        assert_eq!(mods.selection, ["ant".to_owned(), "cat".to_owned()].into());
        // If the anchor no longer exists, treat a Shift-click like a normal click.
        let remaining = shown(&rows[2..]);
        mods.select_installed_row(&remaining[1].0, &remaining, true);
        assert!(!mods.selection.contains("bird"));
        assert!(mods.selection.contains("dog"));
    }

    #[test]
    fn confirmed_empty_installed_resets_filters_selection_and_anchor() {
        let mut mods = ModsState::default();
        mods.row_filter.show = model::Show::On;
        mods.row_filter.makes.insert(model::Make::Content);
        mods.selection.insert("fern".into());
        mods.anchor = Some("sunrise-mods/fern.smod".into());
        let counts = mods.installed_filters(&[]);
        assert!(counts.shows.is_empty() && counts.kinds.is_empty());
        assert_eq!(mods.row_filter, model::RowFilter::default());
        assert!(mods.selection.is_empty() && mods.anchor.is_none());
    }

    #[test]
    fn show_resource_pack_discover_re_searches_after_clearing_a_previous_resource_query() {
        if scratch_child(
            "show_resource_pack_discover_re_searches_after_clearing_a_previous_resource_query",
        )
        .is_none()
        {
            return;
        }
        let mut app = gpui::TestAppContext::single();
        let (launcher, cx) =
            app.add_window_view(|window, cx| Launcher::new_with_startup_checks(window, cx, false));
        launcher.update(cx, |l, _| {
            l.ui.page = Page::Home;
            l.ui.mods.find.set("faithful".into());
            l.ui.mods.packs.scope = pack_discover::Scope::Resource;
            l.ui.mods.packs.status = pack_discover::Status::Done;
            let seq_before = l.ui.mods.packs.seq;

            l.show_resource_pack_discover();

            assert_eq!(l.ui.page, Page::PackFind);
            assert_eq!(l.ui.mods.shelf, Shelf::Resource);
            assert!(l.ui.mods.find.text().is_empty());
            assert!(
                l.ui.mods.packs.seq > seq_before,
                "entering through Find resource packs must trigger a fresh search for the cleared query"
            );
        });
    }

    #[test]
    fn installed_items_key_distinguishes_local_archives_sharing_a_mod_id() {
        let local = [
            model::Local {
                id: "lantern".into(),
                file: "sunrise-mods/lantern-a.smod".into(),
                name: "Lantern A".into(),
                version: "1.0.0".into(),
                github: None,
                native: false,
                served_by: None,
                unreadable: false,
            },
            model::Local {
                id: "lantern".into(),
                file: "sunrise-mods/lantern-b.smod".into(),
                name: "Lantern B".into(),
                version: "1.0.0".into(),
                github: None,
                native: false,
                served_by: None,
                unreadable: false,
            },
        ];
        let rows = model::rows(
            &[],
            &model::World {
                v2: None,
                v2_failure: None,
                build: None,
                unfit_updates: &BTreeSet::new(),
                enabled: &HashSet::new(),
                unbuilt: &HashSet::new(),
                chosen: &BTreeMap::new(),
                local: &local,
            },
        );
        assert_eq!(rows.len(), 2);
        let key_a = installed_items_key(&[InstalledItem::Row(0, 0)], &rows);
        let key_b = installed_items_key(&[InstalledItem::Row(1, 0)], &rows);
        assert_ne!(
            key_a, key_b,
            "swapping local archives with the same mod ID must invalidate cached row heights"
        );
    }
}
