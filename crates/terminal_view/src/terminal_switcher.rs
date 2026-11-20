use std::sync::Arc;

use fuzzy_nucleo::{StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Render, Task,
    WeakEntity, Window, rems,
};
use picker::{Picker, PickerDelegate};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt;
use workspace::{ModalView, Pane, Workspace, item::ItemHandle};

use crate::terminal_panel::TerminalPanel;

const SWITCHER_WIDTH_REMS: f32 = 28.;

pub struct TerminalSwitcher {
    picker: Entity<Picker<TerminalSwitcherDelegate>>,
}

impl ModalView for TerminalSwitcher {}

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
    /// One-based, and only set when the panel is split, to tell apart terminals in different panes.
    pane_number: Option<usize>,
}

pub struct TerminalSwitcherDelegate {
    switcher: WeakEntity<TerminalSwitcher>,
    workspace: WeakEntity<Workspace>,
    candidates: Vec<TerminalCandidate>,
    matches: Vec<StringMatch>,
    selected_index: usize,
    initial_index: usize,
}

impl TerminalSwitcherDelegate {
    fn new(
        switcher: WeakEntity<TerminalSwitcher>,
        workspace: WeakEntity<Workspace>,
        terminal_panel: &Entity<TerminalPanel>,
        cx: &App,
    ) -> Self {
        let terminal_panel = terminal_panel.read(cx);
        let panes = terminal_panel.center.panes();
        let is_split = panes.len() > 1;
        let mut candidates = Vec::new();
        let mut initial_index = 0;
        for (pane_index, pane_handle) in panes.into_iter().enumerate() {
            let pane = pane_handle.read(cx);
            let is_active_pane = *pane_handle == terminal_panel.active_pane;
            let active_item_index = pane.active_item_index();
            for (item_index, item) in pane.items().enumerate() {
                if is_active_pane && item_index == active_item_index {
                    initial_index = candidates.len();
                }
                candidates.push(TerminalCandidate {
                    pane: pane_handle.downgrade(),
                    item: item.boxed_clone(),
                    title: item.tab_content_text(0, cx),
                    pane_number: is_split.then_some(pane_index + 1),
                });
            }
        }

        let matches = Self::all_candidates_as_matches(&candidates);
        Self {
            switcher,
            workspace,
            candidates,
            matches,
            selected_index: initial_index,
            initial_index,
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
                string: candidate.title.clone(),
            })
            .collect()
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
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
        cx.notify();
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
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
                .map(|(id, candidate)| StringMatchCandidate::new(id, &candidate.title))
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
        cx.notify();
        Task::ready(())
    }

    fn confirm(&mut self, _secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(candidate) = self
            .matches
            .get(self.selected_index)
            .and_then(|string_match| self.candidates.get(string_match.candidate_id))
        else {
            return;
        };
        let Some(pane) = candidate.pane.upgrade() else {
            self.dismiss(cx);
            return;
        };

        self.workspace
            .update(cx, |workspace, cx| {
                workspace.focus_panel::<TerminalPanel>(window, cx);
            })
            .log_err();
        let item = candidate.item.boxed_clone();
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
        let string_match = self.matches.get(ix)?;
        let candidate = self.candidates.get(string_match.candidate_id)?;
        Some(
            ListItem::new(ix)
                .spacing(ListItemSpacing::Sparse)
                .inset(true)
                .toggle_state(selected)
                .start_slot(Icon::new(IconName::Terminal).color(Color::Muted))
                .child(HighlightedLabel::new(
                    candidate.title.clone(),
                    string_match.positions.clone(),
                ))
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
