use std::{
    cmp::{self, Reverse},
    sync::Arc,
};

use fuzzy_nucleo::{StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Task,
    WeakEntity, Window, rems,
};
use picker::{Picker, PickerDelegate};
use task::RevealStrategy;
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use util::paths::PathExt;
use workspace::{DismissDecision, ModalView, Pane, Workspace, item::ItemHandle};

use crate::{TerminalView, default_working_directory, terminal_panel::TerminalPanel};

const SWITCHER_WIDTH_REMS: f32 = 28.;

pub struct TerminalSwitcher {
    picker: Entity<Picker<TerminalSwitcherDelegate>>,
}

impl ModalView for TerminalSwitcher {
    fn on_before_dismiss(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> DismissDecision {
        // Every way of closing the switcher (escape, clicking outside it, toggling it again)
        // ends here, so this is where an unconfirmed preview is undone.
        self.picker.update(cx, |picker, cx| {
            picker.delegate.restore_unless_confirmed(window, cx)
        });
        DismissDecision::Dismiss(true)
    }
}

impl EventEmitter<DismissEvent> for TerminalSwitcher {}

impl Focusable for TerminalSwitcher {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for TerminalSwitcher {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("TerminalSwitcher")
            .w(rems(SWITCHER_WIDTH_REMS))
            .child(self.picker.clone())
    }
}

impl TerminalSwitcher {
    pub(crate) fn toggle(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(terminal_panel) = workspace.panel::<TerminalPanel>(cx) else {
            return;
        };
        let weak_workspace = workspace.weak_handle();
        workspace.toggle_modal(window, cx, |window, cx| {
            let delegate = TerminalSwitcherDelegate::new(
                cx.entity().downgrade(),
                weak_workspace,
                &terminal_panel,
                cx,
            );
            Self {
                picker: cx.new(|cx| {
                    Picker::uniform_list(delegate, window, cx)
                        .initial_width(rems(SWITCHER_WIDTH_REMS))
                }),
            }
        });
    }
}

struct TerminalCandidate {
    pane: WeakEntity<Pane>,
    item: Box<dyn ItemHandle>,
    title: SharedString,
    /// The working directory and foreground process, which are searched along with the title.
    detail: Option<SharedString>,
    /// The title and detail joined by a space, so match positions below `title.len()` fall in
    /// the title and the rest fall in the detail.
    search_text: String,
    /// One-based, and only set when the panel is split, to tell apart terminals in different panes.
    pane_number: Option<usize>,
}

pub struct TerminalSwitcherDelegate {
    switcher: WeakEntity<TerminalSwitcher>,
    workspace: WeakEntity<Workspace>,
    terminal_panel: WeakEntity<TerminalPanel>,
    /// Ordered by most recent use, with the active terminal first.
    candidates: Vec<TerminalCandidate>,
    matches: Vec<StringMatch>,
    /// Set when a query matches no terminal, adding a row after the matches that creates a
    /// terminal with this title.
    new_terminal_title: Option<String>,
    selected_index: usize,
    initial_index: usize,
    /// Each pane's active terminal when the switcher opened, restored if the switcher is
    /// dismissed without confirming a preview.
    original_active_items: Vec<(WeakEntity<Pane>, Box<dyn ItemHandle>)>,
    confirmed: bool,
}

impl TerminalSwitcherDelegate {
    fn new(
        switcher: WeakEntity<TerminalSwitcher>,
        workspace: WeakEntity<Workspace>,
        terminal_panel: &Entity<TerminalPanel>,
        cx: &App,
    ) -> Self {
        let weak_terminal_panel = terminal_panel.downgrade();
        let terminal_panel = terminal_panel.read(cx);
        let panes = terminal_panel.center.panes();
        let is_split = panes.len() > 1;
        let mut ranked_candidates = Vec::new();
        let mut original_active_items = Vec::new();
        for (pane_index, pane_handle) in panes.into_iter().enumerate() {
            let pane = pane_handle.read(cx);
            let is_active_pane = *pane_handle == terminal_panel.active_pane;
            let active_item_index = pane.active_item_index();
            if let Some(active_item) = pane.active_item() {
                original_active_items.push((pane_handle.downgrade(), active_item));
            }
            for (item_index, item) in pane.items().enumerate() {
                let is_current = is_active_pane && item_index == active_item_index;
                let title = item.tab_content_text(0, cx);
                let terminal_view = item.downcast::<TerminalView>();
                // Ranking by focus rather than pane activation keeps previews, which activate
                // items without focusing them, from reordering the list.
                let last_focused = terminal_view
                    .as_ref()
                    .and_then(|terminal_view| terminal_view.read(cx).last_focused());
                let detail = terminal_view
                    .and_then(|terminal_view| terminal_detail(terminal_view.read(cx), cx));
                let search_text = match &detail {
                    Some(detail) => format!("{title} {detail}"),
                    None => title.to_string(),
                };
                let candidate = TerminalCandidate {
                    pane: pane_handle.downgrade(),
                    item: item.boxed_clone(),
                    title,
                    detail,
                    search_text,
                    pane_number: is_split.then_some(pane_index + 1),
                };
                ranked_candidates.push((is_current, last_focused, candidate));
            }
        }
        // The active terminal goes first even if it was never focused, as with a terminal restored
        // from the previous session.
        ranked_candidates.sort_by_key(|(is_current, last_focused, _)| {
            (Reverse(*is_current), Reverse(*last_focused))
        });
        let candidates = ranked_candidates
            .into_iter()
            .map(|(_, _, candidate)| candidate)
            .collect::<Vec<_>>();
        // Like alt-tab, start on the previously used terminal so confirming right away switches
        // back to it.
        let initial_index = cmp::min(1, candidates.len().saturating_sub(1));

        let matches = Self::all_candidates_as_matches(&candidates);
        Self {
            switcher,
            workspace,
            terminal_panel: weak_terminal_panel,
            candidates,
            matches,
            new_terminal_title: None,
            selected_index: initial_index,
            initial_index,
            original_active_items,
            confirmed: false,
        }
    }

    fn all_candidates_as_matches(candidates: &[TerminalCandidate]) -> Vec<StringMatch> {
        candidates
            .iter()
            .enumerate()
            .map(|(candidate_id, candidate)| StringMatch {
                candidate_id,
                score: 0.,
                positions: Vec::new(),
                string: candidate.search_text.clone().into(),
            })
            .collect()
    }

    fn selected_candidate(&self) -> Option<&TerminalCandidate> {
        self.matches
            .get(self.selected_index)
            .and_then(|string_match| self.candidates.get(string_match.candidate_id))
    }

    /// Shows the selected terminal in its pane without moving focus out of the switcher.
    fn preview_selected(&self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(candidate) = self.selected_candidate() else {
            self.show_original_active_items(window, cx);
            return;
        };
        let Some(pane) = candidate.pane.upgrade() else {
            return;
        };
        let item = candidate.item.boxed_clone();
        pane.update(cx, |pane, cx| {
            if let Some(index) = pane.index_for_item(item.as_ref()) {
                pane.activate_item(index, false, false, window, cx);
            }
        });
    }

    fn restore_unless_confirmed(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if self.confirmed {
            return;
        }
        // Restoring twice is harmless, but marking it done keeps a later dismissal from
        // undoing anything the user changes after this one.
        self.confirmed = true;
        self.show_original_active_items(window, cx);
    }

    fn show_original_active_items(&self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        for (pane, item) in &self.original_active_items {
            let Some(pane) = pane.upgrade() else {
                continue;
            };
            pane.update(cx, |pane, cx| {
                if let Some(index) = pane.index_for_item(item.as_ref()) {
                    pane.activate_item(index, false, false, window, cx);
                }
            });
        }
    }

    fn selected_new_terminal_title(&self) -> Option<&str> {
        if self.selected_index == self.matches.len() {
            self.new_terminal_title.as_deref()
        } else {
            None
        }
    }

    fn create_terminal(
        &mut self,
        title: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.restore_unless_confirmed(window, cx);
        let (Some(workspace), Some(terminal_panel)) =
            (self.workspace.upgrade(), self.terminal_panel.upgrade())
        else {
            self.dismiss(cx);
            return;
        };
        let working_directory = default_working_directory(workspace.read(cx), cx);
        let create_terminal = terminal_panel.update(cx, |terminal_panel, cx| {
            terminal_panel.add_terminal_shell(
                false,
                working_directory,
                RevealStrategy::Always,
                window,
                cx,
            )
        });
        cx.spawn(async move |_, cx| {
            let terminal = create_terminal.await?;
            terminal_panel.update(cx, |terminal_panel, cx| {
                let terminal_view = terminal_panel
                    .center
                    .panes()
                    .into_iter()
                    .flat_map(|pane| pane.read(cx).items_of_type::<TerminalView>())
                    .find(|terminal_view| {
                        terminal_view.read(cx).terminal().entity_id() == terminal.entity_id()
                    });
                if let Some(terminal_view) = terminal_view {
                    terminal_view.update(cx, |terminal_view, cx| {
                        terminal_view.set_custom_title(Some(title), cx)
                    });
                }
            });
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
        self.dismiss(cx);
    }

    fn dismiss(&self, cx: &mut Context<Picker<Self>>) {
        self.switcher
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }
}

impl PickerDelegate for TerminalSwitcherDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "terminal switcher"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search terminals…".into()
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No matching terminals".into())
    }

    fn match_count(&self) -> usize {
        self.matches.len() + usize::from(self.new_terminal_title.is_some())
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn select_on_hover(&self) -> bool {
        // Previewing follows the selection, so selecting on hover would flip the panel between
        // terminals as the mouse crosses the list.
        false
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        self.preview_selected(window, cx);
        cx.notify();
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        if query.is_empty() {
            self.matches = Self::all_candidates_as_matches(&self.candidates);
            self.selected_index = self.initial_index;
        } else {
            let match_candidates = self
                .candidates
                .iter()
                .enumerate()
                .map(|(id, candidate)| StringMatchCandidate::new(id, &candidate.search_text))
                .collect::<Vec<_>>();
            self.matches = fuzzy_nucleo::match_strings(
                &match_candidates,
                &query,
                fuzzy_nucleo::Case::Smart,
                fuzzy_nucleo::LengthPenalty::On,
                match_candidates.len(),
            );
            self.selected_index = 0;
        }
        let trimmed_query = query.trim();
        self.new_terminal_title = (self.matches.is_empty() && !trimmed_query.is_empty())
            .then(|| trimmed_query.to_string());
        self.preview_selected(window, cx);
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(title) = self.selected_new_terminal_title() {
            let title = title.to_string();
            self.create_terminal(title, window, cx);
            return;
        }
        let Some(candidate) = self.selected_candidate() else {
            return;
        };
        let Some(pane) = candidate.pane.upgrade() else {
            self.dismiss(cx);
            return;
        };
        let item = candidate.item.boxed_clone();
        self.confirmed = true;

        self.workspace
            .update(cx, |workspace, cx| {
                workspace.focus_panel::<TerminalPanel>(window, cx);
            })
            .log_err();
        pane.update(cx, |pane, cx| {
            if let Some(index) = pane.index_for_item(item.as_ref()) {
                pane.activate_item(index, true, true, window, cx);
            }
        });
        // Dismissing after the terminal takes focus keeps the modal layer from restoring focus
        // to wherever it was before the switcher opened.
        self.dismiss(cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.dismiss(cx);
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        if ix == self.matches.len() {
            let title = self.new_terminal_title.as_ref()?;
            return Some(
                ListItem::new(ix)
                    .spacing(ListItemSpacing::Sparse)
                    .inset(true)
                    .toggle_state(selected)
                    .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                    .child(Label::new(format!("New terminal “{title}”")).truncate()),
            );
        }
        let string_match = self.matches.get(ix)?;
        let candidate = self.candidates.get(string_match.candidate_id)?;
        let (title_positions, detail_positions) =
            split_match_positions(&string_match.positions, candidate.title.len());
        Some(
            ListItem::new(ix)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .start_slot(Icon::new(IconName::Terminal).color(Color::Muted))
                .child(
                    v_flex()
                        .min_w_0()
                        .child(HighlightedLabel::new(
                            candidate.title.clone(),
                            title_positions,
                        ))
                        .when_some(candidate.detail.clone(), |this, detail| {
                            this.child(
                                HighlightedLabel::new(detail, detail_positions)
                                    .color(Color::Muted)
                                    .size(LabelSize::Small)
                                    .truncate(),
                            )
                        }),
                )
                .when_some(candidate.pane_number, |this, pane_number| {
                    this.end_slot(
                        Label::new(format!("Pane {pane_number}"))
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                }),
        )
    }
}

fn terminal_detail(terminal_view: &TerminalView, cx: &App) -> Option<SharedString> {
    let terminal = terminal_view.terminal().read(cx);
    let working_directory = terminal
        .working_directory()
        .map(|directory| directory.compact().to_string_lossy().into_owned());
    let process = terminal.foreground_process_command_name();
    match (working_directory, process) {
        (Some(directory), Some(process)) => Some(format!("{directory} · {process}").into()),
        (Some(directory), None) => Some(directory.into()),
        (None, Some(process)) => Some(process.into()),
        (None, None) => None,
    }
}

/// Splits match positions in a candidate's search text, its title and detail joined by a space,
/// into positions within the title and positions within the detail.
fn split_match_positions(positions: &[usize], title_len: usize) -> (Vec<usize>, Vec<usize>) {
    let detail_start = title_len + 1;
    let title_positions = positions
        .iter()
        .copied()
        .filter(|&position| position < title_len)
        .collect();
    let detail_positions = positions
        .iter()
        .filter_map(|&position| position.checked_sub(detail_start))
        .collect();
    (title_positions, detail_positions)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_split_match_positions() {
        let title = "server";
        let detail = "~/dev/zed · cargo";
        let search_text = format!("{title} {detail}");
        let positions = vec![0, 1, 6, 7, 9, search_text.find("cargo").unwrap_or_default()];

        let (title_positions, detail_positions) = split_match_positions(&positions, title.len());

        assert_eq!(title_positions, vec![0, 1]);
        assert_eq!(
            detail_positions,
            vec![0, 2, detail.find("cargo").unwrap_or_default()],
            "positions after the separating space should be shifted into the detail, and a \
             match on the space itself dropped"
        );
    }
}
