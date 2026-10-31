use std::{cmp, path::PathBuf, process::ExitStatus, sync::Arc, time::Duration};

use crate::{
    TerminalView, default_working_directory,
    persistence::{
        SerializedItems, SerializedTerminalPanel, deserialize_terminal_panel, serialize_pane_group,
    },
};
use breadcrumbs::Breadcrumbs;
use collections::HashMap;
use db::kvp::KeyValueStore;
use futures::{channel::oneshot, future::join_all};
use gpui::{
    Action, Anchor, App, AsyncApp, AsyncWindowContext, Context, DragMoveEvent, Entity,
    EventEmitter, FocusHandle, Focusable, IntoElement, MouseButton, MouseUpEvent, ParentElement,
    Pixels, Render, Styled, Subscription, Task, TaskExt, WeakEntity, Window, actions, deferred,
};
use itertools::Itertools;
use project::{Fs, Project};

use settings::{Settings, SettingsStore, TerminalDockPosition, TerminalTabsLocation};
use task::{RevealStrategy, RevealTarget, Shell, ShellBuilder, SpawnInTerminal, TaskId};
use terminal::{Terminal, terminal_settings::TerminalSettings};
use ui::{
    ButtonLike, Clickable, CommonAnimationExt, ContextMenu, FluentBuilder, ListItem, PopoverMenu,
    SplitButton, Tab, Toggleable, Tooltip, prelude::*, right_click_menu,
};
use util::{ResultExt, TryFutureExt, defer};
use workspace::{
    ActivateNextPane, ActivatePane, ActivatePaneDown, ActivatePaneLeft, ActivatePaneRight,
    ActivatePaneUp, ActivatePreviousPane, DraggedTab, ItemId, MoveItemToPane,
    MoveItemToPaneInDirection, MovePaneDown, MovePaneLeft, MovePaneRight, MovePaneUp, Pane,
    PaneGroup, SaveIntent, SplitDirection, SplitDown, SplitLeft, SplitMode, SplitRight, SplitUp,
    SwapPaneDown, SwapPaneLeft, SwapPaneRight, SwapPaneUp, ToggleZoom, Workspace,
    dock::{DockPosition, Panel, PanelEvent, PanelHandle},
    item::{SerializableItem, TabContentParams},
    move_active_item, pane,
};

use anyhow::{Result, anyhow};
use zed_actions::assistant::InlineAssist;

const TERMINAL_PANEL_KEY: &str = "TerminalPanel";
const DEFAULT_TERMINAL_LIST_WIDTH: Pixels = px(200.);
const MIN_TERMINAL_LIST_WIDTH: Pixels = px(100.);
const MIN_TERMINAL_WIDTH_BESIDE_LIST: Pixels = px(200.);
const TERMINAL_LIST_RESIZE_HANDLE_WIDTH: Pixels = px(6.);

actions!(
    terminal_panel,
    [
        /// Toggles the terminal panel.
        Toggle,
        /// Toggles focus on the terminal panel.
        ToggleFocus
    ]
);

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _: &mut Context<Workspace>| {
            workspace.register_action(TerminalPanel::new_terminal);
            workspace.register_action(TerminalPanel::open_terminal);
            workspace.register_action(|workspace, _: &ToggleFocus, window, cx| {
                if is_enabled_in_workspace(workspace, cx) {
                    workspace.toggle_panel_focus::<TerminalPanel>(window, cx);
                }
            });
            workspace.register_action(|workspace, _: &Toggle, window, cx| {
                if is_enabled_in_workspace(workspace, cx) {
                    if !workspace.toggle_panel_focus::<TerminalPanel>(window, cx) {
                        workspace.close_panel::<TerminalPanel>(window, cx);
                    }
                }
            });
        },
    )
    .detach();
}

pub struct TerminalPanel {
    pub(crate) active_pane: Entity<Pane>,
    pub(crate) center: PaneGroup,
    focus_handle: FocusHandle,
    fs: Arc<dyn Fs>,
    workspace: WeakEntity<Workspace>,
    pending_serialization: Task<Option<()>>,
    pending_terminals_to_add: usize,
    restoring: bool,
    _restoration: Task<()>,
    deferred_tasks: HashMap<TaskId, Task<()>>,
    assistant_enabled: bool,
    active: bool,
    terminal_list_visible: bool,
    terminal_list_width: Pixels,
    _settings_subscription: Subscription,
}

#[derive(Clone)]
struct DraggedTerminalListResize;

impl Render for DraggedTerminalListResize {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        gpui::Empty
    }
}

impl TerminalPanel {
    pub fn new(workspace: &Workspace, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let project = workspace.project();
        let pane = new_terminal_pane(workspace.weak_handle(), project.clone(), false, window, cx);
        let center = PaneGroup::new(pane.clone());
        let settings_subscription = cx.observe_global::<SettingsStore>(|terminal_panel, cx| {
            terminal_panel.update_terminal_list_visibility(cx);
        });
        let terminal_panel = Self {
            center,
            active_pane: pane,
            focus_handle: cx.focus_handle(),
            fs: workspace.app_state().fs.clone(),
            workspace: workspace.weak_handle(),
            pending_serialization: Task::ready(None),
            pending_terminals_to_add: 0,
            restoring: false,
            _restoration: Task::ready(()),
            deferred_tasks: HashMap::default(),
            assistant_enabled: false,
            active: false,
            terminal_list_visible: false,
            terminal_list_width: DEFAULT_TERMINAL_LIST_WIDTH,
            _settings_subscription: settings_subscription,
        };
        terminal_panel.apply_tab_bar_buttons(&terminal_panel.active_pane, cx);
        terminal_panel
    }

    pub fn set_assistant_enabled(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.assistant_enabled = enabled;
        for pane in self.center.panes() {
            self.apply_tab_bar_buttons(pane, cx);
        }
    }

    pub(crate) fn apply_tab_bar_buttons(
        &self,
        terminal_pane: &Entity<Pane>,
        cx: &mut Context<Self>,
    ) {
        let assistant_enabled = self.assistant_enabled;
        terminal_pane.update(cx, |pane, cx| {
            pane.set_render_tab_bar_buttons(cx, move |pane, window, cx| {
                let has_focused_rename_editor = pane
                    .active_item()
                    .and_then(|item| item.downcast::<TerminalView>())
                    .is_some_and(|view| view.read(cx).rename_editor_is_focused(window, cx));
                if !pane.has_focus(window, cx)
                    && !pane.context_menu_focused(window, cx)
                    && !has_focused_rename_editor
                {
                    return (None, None);
                }
                let right_children =
                    render_pane_buttons(pane, cx.entity(), assistant_enabled, cx).into();
                (None, right_children)
            });
        });
    }

    fn terminal_count(&self, cx: &App) -> usize {
        self.center
            .panes()
            .into_iter()
            .map(|pane| pane.read(cx).items_len())
            .sum()
    }

    /// Visibility is cached rather than computed on demand because the panes' tab bar
    /// predicates read it while a pane is rendering, when that pane cannot be read to count
    /// its terminals.
    fn update_terminal_list_visibility(&mut self, cx: &mut Context<Self>) {
        let visible = TerminalSettings::get_global(cx).tabs != TerminalTabsLocation::Top
            && self.terminal_count(cx) > 1;
        if visible == self.terminal_list_visible {
            return;
        }
        self.terminal_list_visible = visible;
        for pane in self.center.panes() {
            pane.update(cx, |_, cx| cx.notify());
        }
        cx.notify();
    }

    fn resize_terminal_list(
        &mut self,
        width: Pixels,
        available_width: Pixels,
        cx: &mut Context<Self>,
    ) {
        let max_width =
            (available_width - MIN_TERMINAL_WIDTH_BESIDE_LIST).max(MIN_TERMINAL_LIST_WIDTH);
        self.set_terminal_list_width(width.max(MIN_TERMINAL_LIST_WIDTH).min(max_width), cx);
    }

    fn set_terminal_list_width(&mut self, width: Pixels, cx: &mut Context<Self>) {
        let width = width.round();
        if width != self.terminal_list_width {
            self.terminal_list_width = width;
            self.serialize(cx);
            cx.notify();
        }
    }

    fn serialization_key(workspace: &Workspace) -> Option<String> {
        workspace
            .database_id()
            .map(|id| i64::from(id).to_string())
            .or(workspace.session_id())
            .map(|id| format!("{:?}-{:?}", TERMINAL_PANEL_KEY, id))
    }

    pub async fn load(
        workspace: WeakEntity<Workspace>,
        mut cx: AsyncWindowContext,
    ) -> Result<Entity<Self>> {
        let terminal_panel = workspace.update_in(&mut cx, |workspace, window, cx| {
            cx.new(|cx| TerminalPanel::new(workspace, window, cx))
        })?;

        workspace
            .update(&mut cx, |workspace, _| {
                workspace.set_terminal_provider(TerminalProvider(terminal_panel.clone()))
            })
            .ok();

        terminal_panel.update_in(&mut cx, |panel, window, cx| {
            panel.restoring = true;
            panel._restoration = cx.spawn_in(window, {
                let workspace = workspace.clone();
                async move |terminal_panel, cx| {
                    let restored =
                        Self::restore_serialized_state(workspace, terminal_panel.clone(), cx)
                            .await
                            .log_err()
                            .unwrap_or(false);
                    let default_shell_task = terminal_panel
                        .update_in(cx, |terminal_panel, window, cx| {
                            terminal_panel.finish_restoration(restored, window, cx)
                        })
                        .ok()
                        .flatten();
                    if let Some(task) = default_shell_task {
                        task.await.log_err();
                    }
                }
            });
        })?;

        Ok(terminal_panel)
    }

    fn finish_restoration(
        &mut self,
        restored: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<Result<WeakEntity<Terminal>>>> {
        self.restoring = false;
        let has_terminals = self
            .center
            .panes()
            .into_iter()
            .any(|pane| pane.read(cx).items_len() > 0);
        if restored || has_terminals {
            self.serialize(cx);
        }
        cx.notify();
        if self.active && self.has_no_terminals(cx) {
            let working_directory = self
                .workspace
                .update(cx, |workspace, cx| default_working_directory(workspace, cx))
                .ok()
                .flatten();
            Some(self.add_terminal_shell(
                false,
                working_directory,
                RevealStrategy::Always,
                window,
                cx,
            ))
        } else {
            None
        }
    }

    async fn restore_serialized_state(
        workspace: WeakEntity<Workspace>,
        terminal_panel: WeakEntity<Self>,
        cx: &mut AsyncWindowContext,
    ) -> Result<bool> {
        let mut restored = false;
        if let Some((database_id, serialization_key, kvp)) = workspace
            .read_with(cx, |workspace, cx| {
                workspace
                    .database_id()
                    .zip(TerminalPanel::serialization_key(workspace))
                    .map(|(id, key)| (id, key, KeyValueStore::global(cx)))
            })
            .ok()
            .flatten()
            && let Some(serialized_panel) = cx
                .background_spawn(async move { kvp.read_kvp(&serialization_key) })
                .await
                .log_err()
                .flatten()
                .map(|panel| serde_json::from_str::<SerializedTerminalPanel>(&panel))
                .transpose()
                .log_err()
                .flatten()
        {
            if let Some(terminal_list_width) = serialized_panel.terminal_list_width {
                terminal_panel
                    .update(cx, |terminal_panel, cx| {
                        terminal_panel.terminal_list_width =
                            px(terminal_list_width).max(MIN_TERMINAL_LIST_WIDTH).round();
                        cx.notify();
                    })
                    .log_err();
            }
            let started_at = std::time::Instant::now();
            let deserialized = workspace
                .update_in(cx, |workspace, window, cx| {
                    deserialize_terminal_panel(
                        workspace.weak_handle(),
                        workspace.project().clone(),
                        database_id,
                        serialized_panel,
                        terminal_panel.clone(),
                        window,
                        cx,
                    )
                })?
                .await;
            if let Some(restored_terminals) = deserialized.log_err() {
                restored = restored_terminals > 0;
                log::debug!(
                    "terminal panel: restored {restored_terminals} serialized terminal(s) in {:?}",
                    started_at.elapsed()
                );
            }
        }

        // Since panels/docks are loaded outside from the workspace, we cleanup here, instead of through the workspace.
        let cleanup = workspace.update_in(cx, |workspace, window, cx| {
            let alive_item_ids = terminal_panel.upgrade().map(|terminal_panel| {
                terminal_panel
                    .read(cx)
                    .center
                    .panes()
                    .into_iter()
                    .flat_map(|pane| pane.read(cx).items())
                    .map(|item| item.item_id().as_u64() as ItemId)
                    .collect::<Vec<_>>()
            });
            alive_item_ids
                .zip(workspace.database_id())
                .map(|(alive_item_ids, workspace_id)| {
                    let cleanup_task =
                        TerminalView::cleanup(workspace_id, alive_item_ids.clone(), window, cx);
                    (cleanup_task, alive_item_ids)
                })
        })?;
        if let Some((cleanup_task, alive_item_ids)) = cleanup {
            cleanup_task.await.log_err();
            terminal_panel
                .update(cx, |terminal_panel, cx| {
                    let terminals_to_reserialize = terminal_panel
                        .center
                        .panes()
                        .into_iter()
                        .flat_map(|pane| {
                            pane.read(cx)
                                .items()
                                .filter(|item| {
                                    !alive_item_ids.contains(&(item.item_id().as_u64() as ItemId))
                                })
                                .filter_map(|item| item.act_as::<TerminalView>(cx))
                                .collect::<Vec<_>>()
                        })
                        .collect::<Vec<_>>();
                    for terminal_view in terminals_to_reserialize {
                        terminal_view.update(cx, |terminal_view, cx| {
                            terminal_view.mark_needs_serialize(cx)
                        });
                    }
                })
                .ok();
        }

        let should_focus = workspace
            .update_in(cx, |workspace, window, cx| {
                !workspace.has_active_modal(window, cx)
                    && terminal_panel.upgrade().is_some_and(|terminal_panel| {
                        workspace.active_item(cx).is_none()
                            && workspace
                                .is_dock_at_position_open(terminal_panel.position(window, cx), cx)
                    })
            })
            .unwrap_or(false);
        if should_focus {
            terminal_panel
                .update_in(cx, |panel, window, cx| {
                    panel.active_pane.update(cx, |pane, cx| {
                        pane.focus_active_item(window, cx);
                    });
                })
                .ok();
        }
        Ok(restored)
    }

    fn handle_pane_event(
        &mut self,
        pane: &Entity<Pane>,
        event: &pane::Event,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            pane::Event::ActivateItem { .. } => self.serialize(cx),
            pane::Event::RemovedItem { .. } => {
                self.update_terminal_list_visibility(cx);
                self.serialize(cx);
            }
            pane::Event::Remove { focus_on_pane } => {
                let pane_count_before_removal = self.center.panes().len();
                let _removal_result = self.center.remove(pane, cx);
                self.update_terminal_list_visibility(cx);
                if pane_count_before_removal == 1 {
                    self.center.first_pane().update(cx, |pane, cx| {
                        pane.set_zoomed(false, cx);
                    });
                    cx.emit(PanelEvent::Close);
                } else if let Some(focus_on_pane) =
                    focus_on_pane.as_ref().or_else(|| self.center.panes().pop())
                {
                    focus_on_pane.focus_handle(cx).focus(window, cx);
                }
            }
            pane::Event::ZoomIn => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| {
                        pane.set_zoomed(true, cx);
                    })
                }
                cx.emit(PanelEvent::ZoomIn);
                cx.notify();
            }
            pane::Event::ZoomOut => {
                for pane in self.center.panes() {
                    pane.update(cx, |pane, cx| {
                        pane.set_zoomed(false, cx);
                    })
                }
                cx.emit(PanelEvent::ZoomOut);
                cx.notify();
            }
            pane::Event::AddItem { item } => {
                if let Some(workspace) = self.workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        item.added_to_pane(workspace, pane.clone(), window, cx)
                    })
                }
                self.update_terminal_list_visibility(cx);
                self.serialize(cx);
            }
            &pane::Event::Split { direction, mode } => {
                match mode {
                    SplitMode::ClonePane | SplitMode::EmptyPane => {
                        let clone = matches!(mode, SplitMode::ClonePane);
                        let new_pane = self.new_pane_with_active_terminal(clone, window, cx);
                        let pane = pane.clone();
                        cx.spawn_in(window, async move |panel, cx| {
                            let Some(new_pane) = new_pane.await else {
                                return;
                            };
                            panel
                                .update_in(cx, |panel, window, cx| {
                                    panel.center.split(&pane, &new_pane, direction, cx);
                                    window.focus(&new_pane.focus_handle(cx), cx);
                                })
                                .ok();
                        })
                        .detach();
                    }
                    SplitMode::MovePane => {
                        let Some(item) =
                            pane.update(cx, |pane, cx| pane.take_active_item(window, cx))
                        else {
                            return;
                        };
                        let Ok(project) = self
                            .workspace
                            .update(cx, |workspace, _| workspace.project().clone())
                        else {
                            return;
                        };
                        let new_pane =
                            new_terminal_pane(self.workspace.clone(), project, false, window, cx);
                        new_pane.update(cx, |pane, cx| {
                            pane.add_item(item, true, true, None, window, cx);
                        });
                        self.center.split(&pane, &new_pane, direction, cx);
                        window.focus(&new_pane.focus_handle(cx), cx);
                    }
                };
            }
            pane::Event::Focus => {
                self.active_pane = pane.clone();
            }
            pane::Event::ItemPinned | pane::Event::ItemUnpinned => {
                self.serialize(cx);
            }

            _ => {}
        }
    }

    fn new_pane_with_active_terminal(
        &mut self,
        clone: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Pane>>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(None);
        };
        let workspace = workspace.read(cx);
        let database_id = workspace.database_id();
        let weak_workspace = self.workspace.clone();
        let project = workspace.project().clone();
        let active_pane = &self.active_pane;
        let terminal_view = if clone {
            active_pane
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
        } else {
            None
        };
        let working_directory = if clone {
            terminal_view
                .as_ref()
                .and_then(|terminal_view| {
                    terminal_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .working_directory()
                })
                .or_else(|| default_working_directory(workspace, cx))
        } else {
            default_working_directory(workspace, cx)
        };

        let is_zoomed = if clone {
            active_pane.read(cx).is_zoomed()
        } else {
            false
        };
        cx.spawn_in(window, async move |panel, cx| {
            let terminal = project
                .update(cx, |project, cx| match terminal_view {
                    Some(view) => project.clone_terminal(
                        &view.read(cx).terminal.clone(),
                        cx,
                        working_directory,
                    ),
                    None => project.create_terminal_shell(working_directory, cx),
                })
                .await
                .log_err()?;

            panel
                .update_in(cx, move |terminal_panel, window, cx| {
                    let terminal_view = Box::new(cx.new(|cx| {
                        TerminalView::new(
                            terminal.clone(),
                            weak_workspace.clone(),
                            database_id,
                            project.downgrade(),
                            window,
                            cx,
                        )
                    }));
                    let pane = new_terminal_pane(weak_workspace, project, is_zoomed, window, cx);
                    terminal_panel.apply_tab_bar_buttons(&pane, cx);
                    pane.update(cx, |pane, cx| {
                        pane.add_item(terminal_view, true, true, None, window, cx);
                    });
                    Some(pane)
                })
                .ok()
                .flatten()
        })
    }

    pub fn open_terminal(
        workspace: &mut Workspace,
        action: &workspace::OpenTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let Some(terminal_panel) = workspace.panel::<Self>(cx) else {
            return;
        };

        terminal_panel
            .update(cx, |panel, cx| {
                panel.add_terminal_shell(
                    action.local,
                    Some(action.working_directory.clone()),
                    RevealStrategy::Always,
                    window,
                    cx,
                )
            })
            .detach_and_log_err(cx);
    }

    pub fn spawn_task(
        &mut self,
        task: &SpawnInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Task::ready(Err(anyhow!("failed to read workspace")));
        };

        let project = workspace.read(cx).project().read(cx);

        if project.is_via_collab() {
            return Task::ready(Err(anyhow!("cannot spawn tasks as a guest")));
        }

        let remote_client = project.remote_client();
        let is_windows = project.path_style(cx).is_windows();
        let remote_shell = remote_client
            .as_ref()
            .and_then(|remote_client| remote_client.read(cx).shell());

        let shell = if let Some(remote_shell) = remote_shell
            && task.shell == Shell::System
        {
            Shell::Program(remote_shell)
        } else {
            task.shell.clone()
        };

        let task = prepare_task_for_spawn(task, &shell, is_windows);

        if task.allow_concurrent_runs && task.use_new_terminal {
            return self.spawn_in_new_terminal(task, window, cx);
        }

        let mut terminals_for_task = self.terminals_for_task(&task.full_label, cx);
        let Some(existing) = terminals_for_task.pop() else {
            return self.spawn_in_new_terminal(task, window, cx);
        };

        let (existing_item_index, task_pane, existing_terminal) = existing;
        if task.allow_concurrent_runs {
            return self.replace_terminal(
                task,
                task_pane,
                existing_item_index,
                existing_terminal,
                window,
                cx,
            );
        }

        let (tx, rx) = oneshot::channel();

        self.deferred_tasks.insert(
            task.id.clone(),
            cx.spawn_in(window, async move |terminal_panel, cx| {
                wait_for_terminals_tasks(terminals_for_task, cx).await;
                let task = terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                    if task.use_new_terminal {
                        terminal_panel.spawn_in_new_terminal(task, window, cx)
                    } else {
                        terminal_panel.replace_terminal(
                            task,
                            task_pane,
                            existing_item_index,
                            existing_terminal,
                            window,
                            cx,
                        )
                    }
                });
                if let Ok(task) = task {
                    tx.send(task.await).ok();
                }
            }),
        );

        cx.spawn(async move |_, _| rx.await?)
    }

    fn spawn_in_new_terminal(
        &mut self,
        spawn_task: SpawnInTerminal,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let reveal = spawn_task.reveal;
        let reveal_target = spawn_task.reveal_target;
        match reveal_target {
            RevealTarget::Center => self
                .workspace
                .update(cx, |workspace, cx| {
                    Self::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_task(spawn_task, cx)
                    })
                })
                .unwrap_or_else(|e| Task::ready(Err(e))),
            RevealTarget::Dock => self.add_terminal_task(spawn_task, reveal, window, cx),
        }
    }

    /// Create a new Terminal in the current working directory or the user's home directory
    fn new_terminal(
        workspace: &mut Workspace,
        action: &workspace::NewTerminal,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let center_pane = workspace.active_pane();
        let center_pane_has_focus = center_pane.focus_handle(cx).contains_focused(window, cx);
        let active_center_item_is_terminal = center_pane
            .read(cx)
            .active_item()
            .is_some_and(|item| item.downcast::<TerminalView>().is_some());

        if center_pane_has_focus && active_center_item_is_terminal {
            let working_directory = default_working_directory(workspace, cx);
            let local = action.local;
            Self::add_center_terminal(workspace, window, cx, move |project, cx| {
                if local {
                    project.create_local_terminal(cx)
                } else {
                    project.create_terminal_shell(working_directory, cx)
                }
            })
            .detach_and_log_err(cx);
            return;
        }

        let Some(terminal_panel) = workspace.panel::<Self>(cx) else {
            return;
        };

        terminal_panel
            .update(cx, |this, cx| {
                this.add_terminal_shell(
                    action.local,
                    default_working_directory(workspace, cx),
                    RevealStrategy::Always,
                    window,
                    cx,
                )
            })
            .detach_and_log_err(cx);
    }

    fn terminals_for_task(
        &self,
        label: &str,
        cx: &mut App,
    ) -> Vec<(usize, Entity<Pane>, Entity<TerminalView>)> {
        let Some(workspace) = self.workspace.upgrade() else {
            return Vec::new();
        };

        let pane_terminal_views = |pane: Entity<Pane>| {
            pane.read(cx)
                .items()
                .enumerate()
                .filter_map(|(index, item)| Some((index, item.act_as::<TerminalView>(cx)?)))
                .filter_map(|(index, terminal_view)| {
                    let task_state = terminal_view.read(cx).terminal().read(cx).task()?;
                    if &task_state.spawned_task.full_label == label {
                        Some((index, terminal_view))
                    } else {
                        None
                    }
                })
                .map(move |(index, terminal_view)| (index, pane.clone(), terminal_view))
        };

        self.center
            .panes()
            .into_iter()
            .cloned()
            .flat_map(pane_terminal_views)
            .chain(
                workspace
                    .read(cx)
                    .panes()
                    .iter()
                    .cloned()
                    .flat_map(pane_terminal_views),
            )
            .sorted_by_key(|(_, _, terminal_view)| terminal_view.entity_id())
            .collect()
    }

    fn activate_terminal_view(
        &self,
        pane: &Entity<Pane>,
        item_index: usize,
        focus: bool,
        window: &mut Window,
        cx: &mut App,
    ) {
        pane.update(cx, |pane, cx| {
            pane.activate_item(item_index, true, focus, window, cx)
        })
    }

    pub fn add_center_terminal(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
        create_terminal: impl FnOnce(
            &mut Project,
            &mut Context<Project>,
        ) -> Task<Result<Entity<Terminal>>>
        + 'static,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        if !is_enabled_in_workspace(workspace, cx) {
            return Task::ready(Err(anyhow!(
                "terminal not yet supported for remote projects"
            )));
        }
        let project = workspace.project().downgrade();
        cx.spawn_in(window, async move |workspace, cx| {
            let terminal = project.update(cx, create_terminal)?.await?;

            workspace.update_in(cx, |workspace, window, cx| {
                let terminal_view = cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.weak_handle(),
                        workspace.database_id(),
                        workspace.project().downgrade(),
                        window,
                        cx,
                    )
                });
                // Don't steal focus from an open modal (e.g. the command palette):
                // a background terminal can finish starting up after the user has
                // moved on, and focusing it would dismiss whatever they opened.
                let focus_item = !workspace.has_active_modal(window, cx);
                workspace.add_item_to_active_pane(
                    Box::new(terminal_view),
                    None,
                    focus_item,
                    window,
                    cx,
                );
            })?;
            Ok(terminal.downgrade())
        })
    }

    pub fn add_terminal_task(
        &mut self,
        task: SpawnInTerminal,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let workspace = self.workspace.clone();
        self.spawn_pending_terminal(window, cx, async move |terminal_panel, cx| {
            if workspace.update(cx, |workspace, cx| !is_enabled_in_workspace(workspace, cx))? {
                anyhow::bail!("terminal not yet supported for remote projects");
            }
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            let terminal = project
                .update(cx, |project, cx| project.create_terminal_task(task, cx))
                .await?;
            let pane = terminal_panel
                .read_with(cx, |terminal_panel, _| terminal_panel.active_pane.clone())?;
            workspace.update_in(cx, |workspace, window, cx| {
                let terminal_view = Box::new(cx.new(|cx| {
                    TerminalView::new(
                        terminal.clone(),
                        workspace.weak_handle(),
                        workspace.database_id(),
                        workspace.project().downgrade(),
                        window,
                        cx,
                    )
                }));

                let take_focus = reveal_strategy == RevealStrategy::Always
                    && !workspace.has_active_modal(window, cx);
                match reveal_strategy {
                    RevealStrategy::Always if take_focus => {
                        workspace.focus_panel::<Self>(window, cx);
                    }
                    RevealStrategy::Always | RevealStrategy::NoFocus => {
                        workspace.open_panel::<Self>(window, cx);
                    }
                    RevealStrategy::Never => {}
                }

                pane.update(cx, |pane, cx| {
                    pane.add_item(terminal_view, true, take_focus, None, window, cx);
                });

                terminal.downgrade()
            })
        })
    }

    fn add_terminal_shell(
        &mut self,
        force_local: bool,
        cwd: Option<PathBuf>,
        reveal_strategy: RevealStrategy,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let workspace = self.workspace.clone();
        self.spawn_pending_terminal(window, cx, async move |terminal_panel, cx| {
            if workspace.update(cx, |workspace, cx| !is_enabled_in_workspace(workspace, cx))? {
                anyhow::bail!("terminal not yet supported for collaborative projects");
            }
            let project = workspace.read_with(cx, |workspace, _| workspace.project().clone())?;
            let terminal = if force_local {
                project
                    .update(cx, |project, cx| project.create_local_terminal(cx))
                    .await
            } else {
                project
                    .update(cx, |project, cx| project.create_terminal_shell(cwd, cx))
                    .await
            };

            let pane = terminal_panel
                .read_with(cx, |terminal_panel, _| terminal_panel.active_pane.clone())?;
            match terminal {
                Ok(terminal) => workspace.update_in(cx, |workspace, window, cx| {
                    let terminal_view = Box::new(cx.new(|cx| {
                        TerminalView::new(
                            terminal.clone(),
                            workspace.weak_handle(),
                            workspace.database_id(),
                            workspace.project().downgrade(),
                            window,
                            cx,
                        )
                    }));

                    let take_focus = reveal_strategy == RevealStrategy::Always
                        && !workspace.has_active_modal(window, cx);
                    match reveal_strategy {
                        RevealStrategy::Always if take_focus => {
                            workspace.focus_panel::<Self>(window, cx);
                        }
                        RevealStrategy::Always | RevealStrategy::NoFocus => {
                            workspace.open_panel::<Self>(window, cx);
                        }
                        RevealStrategy::Never => {}
                    }

                    pane.update(cx, |pane, cx| {
                        pane.add_item(terminal_view, true, take_focus, None, window, cx);
                    });

                    terminal.downgrade()
                }),
                Err(error) => {
                    pane.update_in(cx, |pane, window, cx| {
                        let focus = pane.has_focus(window, cx);
                        let failed_to_spawn = cx.new(|cx| FailedToSpawnTerminal {
                            error: error.to_string(),
                            focus_handle: cx.focus_handle(),
                        });
                        pane.add_item(Box::new(failed_to_spawn), true, focus, None, window, cx);
                    })?;
                    Err(error)
                }
            }
        })
    }

    fn spawn_pending_terminal(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        create_terminal: impl AsyncFnOnce(
            WeakEntity<Self>,
            &mut AsyncWindowContext,
        ) -> Result<WeakEntity<Terminal>>
        + 'static,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        self.pending_terminals_to_add += 1;
        cx.notify();
        let decrement_when_cancelled = defer({
            let terminal_panel = cx.weak_entity();
            let cx = cx.to_async();
            move || {
                cx.spawn(async move |cx| {
                    terminal_panel
                        .update(cx, |terminal_panel, cx| {
                            terminal_panel.finish_pending_terminal(cx)
                        })
                        .ok();
                })
                .detach();
            }
        });
        cx.spawn_in(window, async move |terminal_panel, cx| {
            let result = create_terminal(terminal_panel.clone(), cx).await;
            decrement_when_cancelled.abort();
            terminal_panel
                .update(cx, |terminal_panel, cx| {
                    terminal_panel.finish_pending_terminal(cx);
                    if result.is_ok() {
                        terminal_panel.serialize(cx);
                    }
                })
                .ok();
            result
        })
    }

    fn finish_pending_terminal(&mut self, cx: &mut Context<Self>) {
        self.pending_terminals_to_add = self.pending_terminals_to_add.saturating_sub(1);
        cx.notify();
    }

    fn serialize(&mut self, cx: &mut Context<Self>) {
        if self.restoring {
            return;
        }
        let Some(serialization_key) = self
            .workspace
            .read_with(cx, |workspace, _| {
                TerminalPanel::serialization_key(workspace)
            })
            .ok()
            .flatten()
        else {
            return;
        };
        let kvp = KeyValueStore::global(cx);
        self.pending_serialization = cx.spawn(async move |terminal_panel, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(50))
                .await;
            let terminal_panel = terminal_panel.upgrade()?;
            let (items, terminal_list_width) = terminal_panel.update(cx, |terminal_panel, cx| {
                (
                    SerializedItems::WithSplits(serialize_pane_group(
                        &terminal_panel.center,
                        &terminal_panel.active_pane,
                        cx,
                    )),
                    terminal_panel.terminal_list_width.as_f32(),
                )
            });
            cx.background_spawn(
                async move {
                    kvp.write_kvp(
                        serialization_key,
                        serde_json::to_string(&SerializedTerminalPanel {
                            items,
                            active_item_id: None,
                            terminal_list_width: Some(terminal_list_width),
                        })?,
                    )
                    .await?;
                    anyhow::Ok(())
                }
                .log_err(),
            )
            .await;
            Some(())
        });
    }

    fn replace_terminal(
        &self,
        spawn_task: SpawnInTerminal,
        task_pane: Entity<Pane>,
        terminal_item_index: usize,
        terminal_to_replace: Entity<TerminalView>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Result<WeakEntity<Terminal>>> {
        let reveal = spawn_task.reveal;
        let task_workspace = self.workspace.clone();
        cx.spawn_in(window, async move |terminal_panel, cx| {
            let project = terminal_panel.update(cx, |this, cx| {
                this.workspace
                    .update(cx, |workspace, _| workspace.project().clone())
            })??;
            let new_terminal = project
                .update(cx, |project, cx| {
                    project.create_terminal_task(spawn_task, cx)
                })
                .await?;
            terminal_to_replace.update_in(cx, |terminal_to_replace, window, cx| {
                terminal_to_replace.set_terminal(new_terminal.clone(), window, cx);
            })?;

            let reveal_target = terminal_panel.update(cx, |panel, _| {
                if panel.center.panes().iter().any(|p| **p == task_pane) {
                    RevealTarget::Dock
                } else {
                    RevealTarget::Center
                }
            })?;

            match reveal {
                RevealStrategy::Always => match reveal_target {
                    RevealTarget::Center => {
                        task_workspace.update_in(cx, |workspace, window, cx| {
                            let did_activate = workspace.activate_item(
                                &terminal_to_replace,
                                true,
                                true,
                                window,
                                cx,
                            );

                            anyhow::ensure!(did_activate, "Failed to retrieve terminal pane");

                            anyhow::Ok(())
                        })??;
                    }
                    RevealTarget::Dock => {
                        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                            terminal_panel.activate_terminal_view(
                                &task_pane,
                                terminal_item_index,
                                true,
                                window,
                                cx,
                            )
                        })?;

                        cx.spawn(async move |cx| {
                            task_workspace
                                .update_in(cx, |workspace, window, cx| {
                                    workspace.focus_panel::<Self>(window, cx)
                                })
                                .ok()
                        })
                        .detach();
                    }
                },
                RevealStrategy::NoFocus => match reveal_target {
                    RevealTarget::Center => {
                        task_workspace.update_in(cx, |workspace, window, cx| {
                            workspace.active_pane().focus_handle(cx).focus(window, cx);
                        })?;
                    }
                    RevealTarget::Dock => {
                        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                            terminal_panel.activate_terminal_view(
                                &task_pane,
                                terminal_item_index,
                                false,
                                window,
                                cx,
                            )
                        })?;

                        cx.spawn(async move |cx| {
                            task_workspace
                                .update_in(cx, |workspace, window, cx| {
                                    workspace.open_panel::<Self>(window, cx)
                                })
                                .ok()
                        })
                        .detach();
                    }
                },
                RevealStrategy::Never => {}
            }

            Ok(new_terminal.downgrade())
        })
    }

    fn has_no_terminals(&self, cx: &App) -> bool {
        self.active_pane.read(cx).items_len() == 0 && self.pending_terminals_to_add == 0
    }

    pub fn assistant_enabled(&self) -> bool {
        self.assistant_enabled
    }

    /// Returns all panes in the terminal panel.
    pub fn panes(&self) -> Vec<&Entity<Pane>> {
        self.center.panes()
    }

    /// Returns all non-empty terminal selections from all terminal views in all panes.
    pub fn terminal_selections(&self, cx: &App) -> Vec<String> {
        self.center
            .panes()
            .iter()
            .flat_map(|pane| {
                pane.read(cx).items().filter_map(|item| {
                    let terminal_view = item.downcast::<crate::TerminalView>()?;
                    terminal_view
                        .read(cx)
                        .terminal()
                        .read(cx)
                        .last_content
                        .selection_text
                        .clone()
                        .filter(|text| !text.is_empty())
                })
            })
            .collect()
    }

    fn is_enabled(&self, cx: &App) -> bool {
        self.workspace
            .upgrade()
            .is_some_and(|workspace| is_enabled_in_workspace(workspace.read(cx), cx))
    }

    fn activate_pane_in_direction(
        &mut self,
        direction: SplitDirection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(pane) = self
            .center
            .find_pane_in_direction(&self.active_pane, direction, cx)
        {
            window.focus(&pane.focus_handle(cx), cx);
        } else {
            self.workspace
                .update(cx, |workspace, cx| {
                    workspace.activate_pane_in_direction(direction, window, cx)
                })
                .ok();
        }
    }

    fn swap_pane_in_direction(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        if let Some(to) = self
            .center
            .find_pane_in_direction(&self.active_pane, direction, cx)
            .cloned()
        {
            self.center.swap(&self.active_pane, &to, cx);
            cx.notify();
        }
    }

    fn render_terminal_list(
        &self,
        tabs_location: TerminalTabsLocation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panes = self.center.panes();
        let is_split = panes.len() > 1;
        let row_count = self.terminal_count(cx);
        let mut rows = Vec::new();
        for pane in panes {
            let pane_is_active = *pane == self.active_pane;
            let (active_item_index, deemphasized, items) = {
                let pane = pane.read(cx);
                (
                    pane.active_item_index(),
                    !pane.has_focus(window, cx),
                    pane.items()
                        .map(|item| item.boxed_clone())
                        .collect::<Vec<_>>(),
                )
            };

            for (item_index, item) in items.into_iter().enumerate() {
                let row_index = rows.len();
                let split_marker = is_split.then(|| {
                    let marker = if row_index == 0 {
                        "┌"
                    } else if row_index + 1 == row_count {
                        "└"
                    } else {
                        "├"
                    };
                    Label::new(marker).color(Color::Muted)
                });
                let item_id = item.item_id();
                let is_visible = item_index == active_item_index;
                let label = item.tab_content(
                    TabContentParams {
                        detail: None,
                        selected: is_visible,
                        preview: false,
                        deemphasized,
                        max_title_len: None,
                        truncate_title_middle: false,
                    },
                    window,
                    cx,
                );

                let close_button =
                    IconButton::new(("terminal-list-close", item_id), IconName::Close)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("Close Terminal"))
                        .on_click({
                            let pane = pane.clone();
                            move |_, window, cx| {
                                cx.stop_propagation();
                                pane.update(cx, |pane, cx| {
                                    pane.close_item_by_id(item_id, SaveIntent::Close, window, cx)
                                })
                                .detach_and_log_err(cx);
                            }
                        });

                let dragged_tab = DraggedTab {
                    pane: pane.clone(),
                    item: item.boxed_clone(),
                    ix: item_index,
                    detail: 0,
                    is_active: is_visible,
                };
                let target_pane = pane.clone();
                let has_context_menu_actions =
                    !item.tab_extra_context_menu_actions(window, cx).is_empty();
                let menu_item = item.boxed_clone();

                let row = div()
                    .id(("terminal-list-row", item_id))
                    .debug_selector(|| format!("terminal-list-item-{row_index}"))
                    .on_drag(dragged_tab, |tab, _, _, cx| cx.new(|_| tab.clone()))
                    .drag_over::<DraggedTab>({
                        let target_pane = target_pane.clone();
                        move |style, dragged_tab: &DraggedTab, _, cx| {
                            let style = style
                                .bg(cx.theme().colors().drop_target_background)
                                .border_color(cx.theme().colors().drop_target_border);
                            // Within a pane, a terminal dragged downwards lands after the
                            // target row; in every other case it lands before it.
                            if dragged_tab.pane == target_pane && dragged_tab.ix < item_index {
                                style.border_b_2()
                            } else if dragged_tab.pane != target_pane || dragged_tab.ix > item_index
                            {
                                style.border_t_2()
                            } else {
                                style
                            }
                        }
                    })
                    .can_drop(|dragged, _, _| {
                        dragged
                            .downcast_ref::<DraggedTab>()
                            .is_some_and(|tab| tab.item.downcast::<TerminalView>().is_some())
                    })
                    .on_drop(move |dragged_tab: &DraggedTab, window, cx| {
                        target_pane.update(cx, |pane, cx| {
                            // A drag that passed over the terminal area may have left a
                            // split direction behind, which would turn this drop into a split.
                            pane.drag_split_direction = None;
                            pane.handle_tab_drop(dragged_tab, item_index, false, window, cx);
                        });
                    })
                    .child(
                        ListItem::new(("terminal-list-item", item_id))
                            .toggle_state(pane_is_active && is_visible)
                            .start_slot::<Label>(split_marker)
                            .child(label)
                            .end_slot_on_hover(close_button)
                            .on_click({
                                let pane = pane.clone();
                                move |event, window, cx| {
                                    if event.click_count() > 1
                                        && let Some((_, rename_action)) = item
                                            .tab_extra_context_menu_actions(window, cx)
                                            .into_iter()
                                            .find(|(label, _)| label.as_ref() == "Rename")
                                    {
                                        item.item_focus_handle(cx).dispatch_action(
                                            &*rename_action,
                                            window,
                                            cx,
                                        );
                                        return;
                                    }
                                    pane.update(cx, |pane, cx| {
                                        if let Some(index) = pane.index_for_item(item.as_ref()) {
                                            pane.activate_item(index, true, true, window, cx);
                                        }
                                    });
                                }
                            }),
                    );

                rows.push(if has_context_menu_actions {
                    right_click_menu(("terminal-list-menu", item_id))
                        .trigger(move |_, _, _| row)
                        .menu(move |window, cx| {
                            let menu_context = menu_item.item_focus_handle(cx);
                            let actions = menu_item.tab_extra_context_menu_actions(window, cx);
                            ContextMenu::build(window, cx, move |menu, _, _| {
                                actions
                                    .into_iter()
                                    .fold(menu, |menu, (label, action)| menu.action(label, action))
                                    .context(menu_context)
                            })
                        })
                        .into_any_element()
                } else {
                    row.into_any_element()
                });
            }
        }

        let header = h_flex()
            .flex_none()
            .h(Tab::container_height(cx))
            .px_1()
            .justify_end()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(render_pane_buttons(
                self.active_pane.read(cx),
                self.active_pane.clone(),
                self.assistant_enabled,
                cx,
            ));

        let resize_handle = div()
            .id("terminal-list-resize-handle")
            .debug_selector(|| "terminal-list-resize-handle".into())
            .absolute()
            .top_0()
            .h_full()
            .w(TERMINAL_LIST_RESIZE_HANDLE_WIDTH)
            // Offsets are measured from inside the list's 1px border, so include it to center the
            // handle on the list's outer edge, where the width is measured from while dragging.
            .map(|this| match tabs_location {
                TerminalTabsLocation::Left => {
                    this.right(-TERMINAL_LIST_RESIZE_HANDLE_WIDTH / 2. - px(1.))
                }
                TerminalTabsLocation::Top | TerminalTabsLocation::Right => {
                    this.left(-TERMINAL_LIST_RESIZE_HANDLE_WIDTH / 2. - px(1.))
                }
            })
            .cursor_col_resize()
            .on_drag(DraggedTerminalListResize, |resize, _, _, cx| {
                cx.stop_propagation();
                cx.new(|_| resize.clone())
            })
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|terminal_panel, event: &MouseUpEvent, _, cx| {
                    if event.click_count == 2 {
                        terminal_panel.set_terminal_list_width(DEFAULT_TERMINAL_LIST_WIDTH, cx);
                        cx.stop_propagation();
                    }
                }),
            )
            .occlude();

        v_flex()
            .relative()
            .flex_none()
            .w(self.terminal_list_width)
            .h_full()
            .bg(cx.theme().colors().panel_background)
            .border_color(cx.theme().colors().border)
            .map(|this| match tabs_location {
                TerminalTabsLocation::Left => this.border_r_1(),
                TerminalTabsLocation::Top | TerminalTabsLocation::Right => this.border_l_1(),
            })
            .child(deferred(resize_handle))
            .child(header)
            .child(
                v_flex()
                    .id("terminal-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .into_any_element()
    }

    fn move_pane_to_border(&mut self, direction: SplitDirection, cx: &mut Context<Self>) {
        if self
            .center
            .move_to_border(&self.active_pane, direction, cx)
            .unwrap()
        {
            cx.notify();
        }
    }
}

/// Prepares a `SpawnInTerminal` by computing the command, args, and command_label
/// based on the shell configuration. This is a pure function that can be tested
/// without spawning actual terminals.
pub fn prepare_task_for_spawn(
    task: &SpawnInTerminal,
    shell: &Shell,
    is_windows: bool,
) -> SpawnInTerminal {
    let builder = ShellBuilder::new(shell, is_windows);
    let command_label = builder.command_label(task.command.as_deref().unwrap_or(""));
    let (command, args) = builder.build_no_quote(task.command.clone(), &task.args);

    SpawnInTerminal {
        command_label,
        command: Some(command),
        args,
        ..task.clone()
    }
}

fn is_enabled_in_workspace(workspace: &Workspace, cx: &App) -> bool {
    workspace.project().read(cx).supports_terminal(cx)
}

pub fn new_terminal_pane(
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    zoomed: bool,
    window: &mut Window,
    cx: &mut Context<TerminalPanel>,
) -> Entity<Pane> {
    let terminal_panel = cx.entity();
    let pane = cx.new(|cx| {
        let mut pane = Pane::new(
            workspace.clone(),
            project.clone(),
            Default::default(),
            None,
            workspace::NewTerminal::default().boxed_clone(),
            false,
            window,
            cx,
        );
        pane.set_zoomed(zoomed, cx);
        pane.set_can_navigate(false, cx);
        pane.display_nav_history_buttons(None);
        let tab_bar_terminal_panel = terminal_panel.downgrade();
        pane.set_should_display_tab_bar(move |_, cx| {
            tab_bar_terminal_panel
                .read_with(cx, |terminal_panel, _| {
                    !terminal_panel.terminal_list_visible
                })
                .unwrap_or(true)
        });
        pane.set_zoom_out_on_close(false);

        let split_closure_terminal_panel = terminal_panel.downgrade();
        pane.set_can_split(Some(Arc::new(move |pane, dragged_item, _window, cx| {
            if let Some(tab) = dragged_item.downcast_ref::<DraggedTab>() {
                let is_current_pane = tab.pane == cx.entity();
                let Some(can_drag_away) = split_closure_terminal_panel
                    .read_with(cx, |terminal_panel, _| {
                        let current_panes = terminal_panel.center.panes();
                        !current_panes.contains(&&tab.pane)
                            || current_panes.len() > 1
                            || (!is_current_pane || pane.items_len() > 1)
                    })
                    .ok()
                else {
                    return false;
                };
                if can_drag_away {
                    let item = if is_current_pane {
                        pane.item_for_index(tab.ix)
                    } else {
                        tab.pane.read(cx).item_for_index(tab.ix)
                    };
                    if let Some(item) = item {
                        return item.downcast::<TerminalView>().is_some();
                    }
                }
            }
            false
        })));

        let toolbar = pane.toolbar().clone();
        if let Some(callbacks) = cx.try_global::<workspace::PaneSearchBarCallbacks>() {
            let languages = Some(project.read(cx).languages().clone());
            (callbacks.setup_search_bar)(languages, &toolbar, window, cx);
        }
        let breadcrumbs = cx.new(|_| Breadcrumbs::new());
        toolbar.update(cx, |toolbar, cx| {
            toolbar.add_item(breadcrumbs, window, cx);
        });

        pane
    });

    cx.subscribe_in(&pane, window, TerminalPanel::handle_pane_event)
        .detach();
    cx.observe(&pane, |_, _, cx| cx.notify()).detach();

    pane
}

async fn wait_for_terminals_tasks(
    terminals_for_task: Vec<(usize, Entity<Pane>, Entity<TerminalView>)>,
    cx: &mut AsyncApp,
) {
    let pending_tasks = terminals_for_task.iter().map(|(_, _, terminal)| {
        terminal.update(cx, |terminal_view, cx| {
            terminal_view
                .terminal()
                .update(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
        })
    });
    join_all(pending_tasks).await;
}

struct FailedToSpawnTerminal {
    error: String,
    focus_handle: FocusHandle,
}

impl Focusable for FailedToSpawnTerminal {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for FailedToSpawnTerminal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let popover_menu = PopoverMenu::new("settings-popover")
            .trigger(
                IconButton::new("icon-button-popover", IconName::ChevronDown)
                    .icon_size(IconSize::XSmall),
            )
            .menu(move |window, cx| {
                Some(ContextMenu::build(window, cx, |context_menu, _, _| {
                    context_menu
                        .action("Open Settings", zed_actions::OpenSettings.boxed_clone())
                        .action(
                            "Edit settings.json",
                            zed_actions::OpenSettingsFile.boxed_clone(),
                        )
                }))
            })
            .anchor(Anchor::TopRight)
            .offset(gpui::Point {
                x: px(0.0),
                y: px(2.0),
            });

        v_flex()
            .track_focus(&self.focus_handle)
            .size_full()
            .p_4()
            .items_center()
            .justify_center()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .max_w_112()
                    .items_center()
                    .justify_center()
                    .text_center()
                    .child(Label::new("Failed to spawn terminal"))
                    .child(
                        Label::new(self.error.to_string())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .mb_4(),
                    )
                    .child(SplitButton::new(
                        ButtonLike::new("open-settings-ui")
                            .child(Label::new("Edit Settings").size(LabelSize::Small))
                            .on_click(|_, window, cx| {
                                window.dispatch_action(zed_actions::OpenSettings.boxed_clone(), cx);
                            }),
                        popover_menu.into_any_element(),
                    )),
            )
    }
}

impl EventEmitter<()> for FailedToSpawnTerminal {}

impl workspace::Item for FailedToSpawnTerminal {
    type Event = ();

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        SharedString::new_static("Failed to spawn terminal")
    }
}

impl EventEmitter<PanelEvent> for TerminalPanel {}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let registrar = cx
            .try_global::<workspace::PaneSearchBarCallbacks>()
            .map(|callbacks| {
                (callbacks.wrap_div_with_search_actions)(div(), self.active_pane.clone())
            })
            .unwrap_or_else(div);
        let no_items_in_panes = self
            .center
            .panes()
            .into_iter()
            .all(|pane| pane.read(cx).items_len() == 0);
        let waiting_for_terminals = self.restoring || self.pending_terminals_to_add > 0;
        let restoring_placeholder = (waiting_for_terminals && no_items_in_panes).then(|| {
            let label = if self.restoring {
                "Restoring terminals…"
            } else {
                "Starting terminal…"
            };
            h_flex()
                .absolute()
                .inset_0()
                .justify_center()
                .gap_2()
                .child(
                    Icon::new(IconName::ArrowCircle)
                        .color(Color::Muted)
                        .size(IconSize::Small)
                        .with_rotate_animation(2),
                )
                .child(Label::new(label).color(Color::Muted))
        });
        let tabs_location = TerminalSettings::get_global(cx).tabs;
        let terminal_list = self.terminal_list_visible.then(|| {
            let terminal_list = self.render_terminal_list(tabs_location, window, cx);
            let resize_terminal_list = cx.listener(
                move |terminal_panel, event: &DragMoveEvent<DraggedTerminalListResize>, _, cx| {
                    let bounds = event.bounds;
                    let width = match tabs_location {
                        TerminalTabsLocation::Left => event.event.position.x - bounds.left(),
                        TerminalTabsLocation::Top | TerminalTabsLocation::Right => {
                            bounds.right() - event.event.position.x
                        }
                    };
                    terminal_panel.resize_terminal_list(width, bounds.size.width, cx);
                },
            );
            (terminal_list, resize_terminal_list)
        });
        self.workspace
            .update(cx, |workspace, cx| {
                let follower_states = HashMap::default();
                let weak_workspace = workspace.weak_handle();
                let render_context = workspace::PaneRenderContext {
                    follower_states: &follower_states,
                    active_call: workspace.active_call(),
                    active_pane: &self.active_pane,
                    app_state: workspace.app_state(),
                    project: workspace.project(),
                    workspace: &weak_workspace,
                };
                let center =
                    self.center
                        .render(workspace.zoomed_item(), None, &render_context, window, cx);
                registrar
                    .track_focus(&self.focus_handle)
                    .size_full()
                    .relative()
                    .map(|this| match terminal_list {
                        None => this.child(center),
                        Some((terminal_list, resize_terminal_list)) => {
                            let center = div().flex_1().min_w_0().h_full().child(center);
                            let content = h_flex().size_full().on_drag_move(resize_terminal_list);
                            this.child(if tabs_location == TerminalTabsLocation::Left {
                                content.child(terminal_list).child(center)
                            } else {
                                content.child(center).child(terminal_list)
                            })
                        }
                    })
                    .children(restoring_placeholder)
            })
            .ok()
            .map(|div| {
                div.on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneLeft, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Left, window, cx);
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneRight, window, cx| {
                        terminal_panel.activate_pane_in_direction(
                            SplitDirection::Right,
                            window,
                            cx,
                        );
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneUp, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Up, window, cx);
                    })
                })
                .on_action({
                    cx.listener(|terminal_panel, _: &ActivatePaneDown, window, cx| {
                        terminal_panel.activate_pane_in_direction(SplitDirection::Down, window, cx);
                    })
                })
                .on_action(
                    cx.listener(|terminal_panel, _action: &ActivateNextPane, window, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(ix) = panes
                            .iter()
                            .position(|pane| **pane == terminal_panel.active_pane)
                        {
                            let next_ix = (ix + 1) % panes.len();
                            window.focus(&panes[next_ix].focus_handle(cx), cx);
                        }
                    }),
                )
                .on_action(cx.listener(
                    |terminal_panel, _action: &ActivatePreviousPane, window, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(ix) = panes
                            .iter()
                            .position(|pane| **pane == terminal_panel.active_pane)
                        {
                            let prev_ix = cmp::min(ix.wrapping_sub(1), panes.len() - 1);
                            window.focus(&panes[prev_ix].focus_handle(cx), cx);
                        }
                    },
                ))
                .on_action(
                    cx.listener(|terminal_panel, action: &ActivatePane, window, cx| {
                        let panes = terminal_panel.center.panes();
                        if let Some(&pane) = panes.get(action.0) {
                            window.focus(&pane.read(cx).focus_handle(cx), cx);
                        } else {
                            let future =
                                terminal_panel.new_pane_with_active_terminal(true, window, cx);
                            cx.spawn_in(window, async move |terminal_panel, cx| {
                                if let Some(new_pane) = future.await {
                                    _ = terminal_panel.update_in(
                                        cx,
                                        |terminal_panel, window, cx| {
                                            terminal_panel.center.split(
                                                &terminal_panel.active_pane,
                                                &new_pane,
                                                SplitDirection::Right,
                                                cx,
                                            );
                                            let new_pane = new_pane.read(cx);
                                            window.focus(&new_pane.focus_handle(cx), cx);
                                        },
                                    );
                                }
                            })
                            .detach();
                        }
                    }),
                )
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneLeft, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Left, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneRight, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Right, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneUp, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Up, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &SwapPaneDown, _, cx| {
                    terminal_panel.swap_pane_in_direction(SplitDirection::Down, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneLeft, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Left, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneRight, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Right, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneUp, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Up, cx);
                }))
                .on_action(cx.listener(|terminal_panel, _: &MovePaneDown, _, cx| {
                    terminal_panel.move_pane_to_border(SplitDirection::Down, cx);
                }))
                .on_action(
                    cx.listener(|terminal_panel, action: &MoveItemToPane, window, cx| {
                        let Some(&target_pane) =
                            terminal_panel.center.panes().get(action.destination)
                        else {
                            return;
                        };
                        move_active_item(
                            &terminal_panel.active_pane,
                            target_pane,
                            action.focus,
                            true,
                            window,
                            cx,
                        );
                    }),
                )
                .on_action(cx.listener(
                    |terminal_panel, action: &MoveItemToPaneInDirection, window, cx| {
                        let source_pane = &terminal_panel.active_pane;
                        if let Some(destination_pane) = terminal_panel
                            .center
                            .find_pane_in_direction(source_pane, action.direction, cx)
                        {
                            move_active_item(
                                source_pane,
                                destination_pane,
                                action.focus,
                                true,
                                window,
                                cx,
                            );
                        };
                    },
                ))
            })
            .unwrap_or_else(|| div())
    }
}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Panel for TerminalPanel {
    fn activation_focus_handle(&self, cx: &App) -> FocusHandle {
        self.active_pane.focus_handle(cx)
    }

    fn position(&self, _window: &Window, cx: &App) -> DockPosition {
        TerminalSettings::get_global(cx).dock.into()
    }

    fn position_is_valid(&self, _: DockPosition) -> bool {
        true
    }

    fn starts_open(&self, _: &Window, cx: &App) -> bool {
        TerminalSettings::get_global(cx).starts_open
    }

    fn set_position(
        &mut self,
        position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            let dock = match position {
                DockPosition::Left => TerminalDockPosition::Left,
                DockPosition::Bottom => TerminalDockPosition::Bottom,
                DockPosition::Right => TerminalDockPosition::Right,
            };
            settings.terminal.get_or_insert_default().dock = Some(dock);
        });
    }

    fn default_size(&self, window: &Window, cx: &App) -> Pixels {
        let settings = TerminalSettings::get_global(cx);
        match self.position(window, cx) {
            DockPosition::Left | DockPosition::Right => settings.default_width,
            DockPosition::Bottom => settings.default_height,
        }
    }

    fn supports_flexible_size(&self) -> bool {
        true
    }

    fn has_flexible_size(&self, _window: &Window, cx: &App) -> bool {
        TerminalSettings::get_global(cx).flexible
    }

    fn set_flexible_size(&mut self, flexible: bool, _window: &mut Window, cx: &mut Context<Self>) {
        settings::update_settings_file(self.fs.clone(), cx, move |settings, _| {
            settings.terminal.get_or_insert_default().flexible = Some(flexible);
        });
    }

    fn is_zoomed(&self, _window: &Window, cx: &App) -> bool {
        self.active_pane.read(cx).is_zoomed()
    }

    fn set_zoomed(&mut self, zoomed: bool, _: &mut Window, cx: &mut Context<Self>) {
        for pane in self.center.panes() {
            pane.update(cx, |pane, cx| {
                pane.set_zoomed(zoomed, cx);
            })
        }
        cx.notify();
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let old_active = self.active;
        self.active = active;
        if !active || old_active == active || self.restoring || !self.has_no_terminals(cx) {
            return;
        }
        cx.defer_in(window, |this, window, cx| {
            let Ok(kind) = this
                .workspace
                .update(cx, |workspace, cx| default_working_directory(workspace, cx))
            else {
                return;
            };

            this.add_terminal_shell(false, kind, RevealStrategy::Always, window, cx)
                .detach_and_log_err(cx)
        })
    }

    fn icon_label(&self, _window: &Window, cx: &App) -> Option<String> {
        if !TerminalSettings::get_global(cx).show_count_badge {
            return None;
        }
        let count = self
            .center
            .panes()
            .into_iter()
            .map(|pane| pane.read(cx).items_len())
            .sum::<usize>();
        if count == 0 {
            None
        } else {
            Some(count.to_string())
        }
    }

    fn persistent_name() -> &'static str {
        "TerminalPanel"
    }

    fn panel_key() -> &'static str {
        TERMINAL_PANEL_KEY
    }

    fn icon(&self, _window: &Window, cx: &App) -> Option<IconName> {
        if (self.is_enabled(cx) || !self.has_no_terminals(cx))
            && TerminalSettings::get_global(cx).button
        {
            Some(IconName::TerminalAlt)
        } else {
            None
        }
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("Terminal Panel")
    }

    fn toggle_action(&self) -> Box<dyn gpui::Action> {
        Box::new(Toggle)
    }

    fn pane(&self) -> Option<Entity<Pane>> {
        Some(self.active_pane.clone())
    }

    fn activation_priority(&self) -> u32 {
        2
    }

    fn hide_button_setting(&self, _: &App) -> Option<workspace::HideStatusItem> {
        Some(workspace::HideStatusItem::new(|settings| {
            settings.terminal.get_or_insert_default().button = Some(false);
        }))
    }
}

struct TerminalProvider(Entity<TerminalPanel>);

impl workspace::TerminalProvider for TerminalProvider {
    fn spawn(
        &self,
        task: SpawnInTerminal,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<Option<Result<ExitStatus>>> {
        let terminal_panel = self.0.clone();
        window.spawn(cx, async move |cx| {
            let terminal = terminal_panel
                .update_in(cx, |terminal_panel, window, cx| {
                    terminal_panel.spawn_task(&task, window, cx)
                })
                .ok()?
                .await;
            match terminal {
                Ok(terminal) => {
                    let exit_status = terminal
                        .read_with(cx, |terminal, cx| terminal.wait_for_completed_task(cx))
                        .ok()?
                        .await?;
                    Some(Ok(exit_status))
                }
                Err(e) => Some(Err(e)),
            }
        })
    }
}

/// Takes the pane by reference because this is also called from the pane's own render, where
/// reading `pane_handle` would panic. `pane_handle` is only used by click handlers.
fn render_pane_buttons(
    pane: &Pane,
    pane_handle: Entity<Pane>,
    assistant_enabled: bool,
    cx: &App,
) -> AnyElement {
    let split_context = pane
        .active_item()
        .and_then(|item| item.downcast::<TerminalView>())
        .map(|terminal_view| terminal_view.read(cx).focus_handle.clone());
    let focus_handle = pane.focus_handle(cx);
    let zoomed = pane.is_zoomed();
    h_flex()
        .gap(DynamicSpacing::Base02.rems(cx))
        .child(
            PopoverMenu::new("terminal-tab-bar-popover-menu")
                .trigger_with_tooltip(
                    IconButton::new("plus", IconName::Plus).icon_size(IconSize::Small),
                    Tooltip::text("New…"),
                )
                .anchor(Anchor::TopRight)
                .with_handle(pane.new_item_context_menu_handle.clone())
                .menu(move |window, cx| {
                    let focus_handle = focus_handle.clone();
                    let menu = ContextMenu::build(window, cx, |menu, _, _| {
                        menu.context(focus_handle.clone())
                            .action(
                                "New Terminal",
                                workspace::NewTerminal::default().boxed_clone(),
                            )
                            // We want the focus to go back to terminal panel once task modal is dismissed,
                            // hence we focus that first. Otherwise, we'd end up without a focused element, as
                            // context menu will be gone the moment we spawn the modal.
                            .action("Spawn Task", zed_actions::Spawn::modal().boxed_clone())
                    });

                    Some(menu)
                }),
        )
        .when(assistant_enabled, |this| {
            this.when_some(split_context.clone(), |this, focus_handle| {
                this.child(InlineAssistTabBarButton { focus_handle })
            })
        })
        .child(
            PopoverMenu::new("terminal-pane-tab-bar-split")
                .trigger_with_tooltip(
                    IconButton::new("terminal-pane-split", IconName::Split)
                        .icon_size(IconSize::Small),
                    Tooltip::text("Split Pane"),
                )
                .anchor(Anchor::TopRight)
                .with_handle(pane.split_item_context_menu_handle.clone())
                .menu({
                    move |window, cx| {
                        ContextMenu::build(window, cx, |menu, _, _| {
                            menu.when_some(split_context.clone(), |menu, split_context| {
                                menu.context(split_context)
                            })
                            .action("Split Right", SplitRight::default().boxed_clone())
                            .action("Split Left", SplitLeft::default().boxed_clone())
                            .action("Split Up", SplitUp::default().boxed_clone())
                            .action("Split Down", SplitDown::default().boxed_clone())
                        })
                        .into()
                    }
                }),
        )
        .child(
            IconButton::new("toggle_zoom", IconName::Maximize)
                .icon_size(IconSize::Small)
                .toggle_state(zoomed)
                .selected_icon(IconName::Minimize)
                .on_click(move |_, window, cx| {
                    pane_handle.update(cx, |pane, cx| {
                        pane.toggle_zoom(&workspace::ToggleZoom, window, cx);
                    });
                })
                .tooltip(move |_window, cx| {
                    Tooltip::for_action(
                        if zoomed { "Zoom Out" } else { "Zoom In" },
                        &ToggleZoom,
                        cx,
                    )
                }),
        )
        .into_any_element()
}

#[derive(IntoElement)]
struct InlineAssistTabBarButton {
    focus_handle: FocusHandle,
}

impl RenderOnce for InlineAssistTabBarButton {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let focus_handle = self.focus_handle;
        IconButton::new("terminal_inline_assistant", IconName::ZedAssistant)
            .icon_size(IconSize::Small)
            .on_click({
                let focus_handle = focus_handle.clone();
                move |_, window, cx| {
                    focus_handle.dispatch_action(&InlineAssist::default(), window, cx);
                }
            })
            .tooltip(move |_window, cx| {
                Tooltip::for_action_in("Inline Assist", &InlineAssist::default(), &focus_handle, cx)
            })
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZero;

    use super::*;
    use crate::persistence::{SerializedPane, SerializedPaneGroup};
    use gpui::{
        EntityId, Modifiers, MouseDownEvent, Point, TestAppContext, UpdateGlobal as _,
        VisualTestContext, point,
    };
    use pretty_assertions::assert_eq;
    use project::FakeFs;
    use settings::SettingsStore;
    use workspace::{MultiWorkspace, WorkspaceId};

    #[test]
    fn test_prepare_empty_task() {
        let input = SpawnInTerminal::default();
        let shell = Shell::System;

        let result = prepare_task_for_spawn(&input, &shell, false);

        let expected_shell = util::get_system_shell();
        assert_eq!(result.env, HashMap::default());
        assert_eq!(result.cwd, None);
        assert_eq!(result.shell, Shell::System);
        assert_eq!(
            result.command,
            Some(expected_shell.clone()),
            "Empty tasks should spawn a -i shell"
        );
        assert_eq!(result.args, Vec::<String>::new());
        assert_eq!(
            result.command_label, expected_shell,
            "We show the shell launch for empty commands"
        );
    }

    #[gpui::test]
    async fn test_bypass_max_tabs_limit(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        set_max_tabs(cx, Some(3));

        for _ in 0..5 {
            let task = window_handle
                .update(cx, |_, window, cx| {
                    terminal_panel.update(cx, |panel, cx| {
                        panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
                    })
                })
                .unwrap();
            task.await.unwrap();
        }

        cx.run_until_parked();

        let item_count =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        assert_eq!(
            item_count, 5,
            "Terminal panel should bypass max_tabs limit and have all 5 terminals"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_prepare_script_like_task() {
        let user_command = r#"REPO_URL=$(git remote get-url origin | sed -e \"s/^git@\\(.*\\):\\(.*\\)\\.git$/https:\\/\\/\\1\\/\\2/\"); COMMIT_SHA=$(git log -1 --format=\"%H\" -- \"${ZED_RELATIVE_FILE}\"); echo \"${REPO_URL}/blob/${COMMIT_SHA}/${ZED_RELATIVE_FILE}#L${ZED_ROW}-$(echo $(($(wc -l <<< \"$ZED_SELECTED_TEXT\") + $ZED_ROW - 1)))\" | xclip -selection clipboard"#.to_string();
        let expected_cwd = PathBuf::from("/some/work");

        let input = SpawnInTerminal {
            command: Some(user_command.clone()),
            cwd: Some(expected_cwd.clone()),
            ..SpawnInTerminal::default()
        };
        let shell = Shell::System;

        let result = prepare_task_for_spawn(&input, &shell, false);

        let system_shell = util::get_system_shell();
        assert_eq!(result.env, HashMap::default());
        assert_eq!(result.cwd, Some(expected_cwd));
        assert_eq!(result.shell, Shell::System);
        assert_eq!(result.command, Some(system_shell.clone()));
        assert_eq!(
            result.args,
            vec!["-i".to_string(), "-c".to_string(), user_command.clone()],
            "User command should have been moved into the arguments, as we're spawning a new -i shell",
        );
        assert_eq!(
            result.command_label,
            format!(
                "{system_shell} {interactive}-c '{user_command}'",
                interactive = if cfg!(windows) { "" } else { "-i " }
            ),
            "We want to show to the user the entire command spawned"
        );
    }

    #[gpui::test]
    async fn renders_error_if_default_shell_fails(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.terminal.get_or_insert_default().project.shell =
                        Some(settings::Shell::Program("__nonexistent_shell__".to_owned()));
                });
            });
        });

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Always,
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
            .await
            .unwrap_err();

        window_handle
            .update(cx, |_, _, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    assert!(
                        terminal_panel
                            .active_pane
                            .read(cx)
                            .items()
                            .any(|item| item.downcast::<FailedToSpawnTerminal>().is_some()),
                        "should spawn `FailedToSpawnTerminal` pane"
                    );
                    assert_eq!(terminal_panel.pending_terminals_to_add, 0);
                })
            })
            .unwrap();
    }

    #[gpui::test]
    async fn test_failed_task_spawn_does_not_leak_pending_terminal_count(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_task(
                        SpawnInTerminal {
                            command: Some("__nonexistent_program__".to_owned()),
                            ..SpawnInTerminal::default()
                        },
                        RevealStrategy::Never,
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
            .await
            .unwrap_err();

        cx.run_until_parked();
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(terminal_panel.pending_terminals_to_add, 0);
            assert_eq!(terminal_panel.active_pane.read(cx).items_len(), 0);
        });
    }

    #[gpui::test]
    async fn test_pending_terminal_count_tracks_spawn_lifecycle(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        let task = window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    let task =
                        terminal_panel.add_terminal_shell(false, None, RevealStrategy::Never, window, cx);
                    assert_eq!(
                        terminal_panel.pending_terminals_to_add, 1,
                        "pending count should be incremented synchronously to avoid double default terminal spawns"
                    );
                    assert!(!terminal_panel.has_no_terminals(cx));
                    task
                })
            })
            .unwrap();
        task.await.unwrap();

        cx.run_until_parked();
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(terminal_panel.pending_terminals_to_add, 0);
            assert_eq!(terminal_panel.active_pane.read(cx).items_len(), 1);
        });
    }

    #[gpui::test]
    async fn test_pending_terminal_count_resets_when_spawn_cancelled(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    let task = terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Never,
                        window,
                        cx,
                    );
                    assert_eq!(terminal_panel.pending_terminals_to_add, 1);
                    drop(task);
                })
            })
            .unwrap();

        cx.run_until_parked();
        terminal_panel.read_with(cx, |terminal_panel, _| {
            assert_eq!(terminal_panel.pending_terminals_to_add, 0);
        });
    }

    #[gpui::test]
    async fn test_load_without_serialized_state_does_not_persist_empty_state(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let serialization_key = window_handle
            .update(cx, |multi_workspace, _, cx| {
                TerminalPanel::serialization_key(multi_workspace.workspace().read(cx)).unwrap()
            })
            .unwrap();

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().downgrade();
                window.spawn(cx, async move |cx| {
                    TerminalPanel::load(workspace, cx.clone()).await
                })
            })
            .unwrap()
            .await
            .unwrap();

        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();

        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert!(!terminal_panel.restoring);
            assert_eq!(terminal_panel.active_pane.read(cx).items_len(), 0);
        });
        let serialized_state = cx
            .update(|cx| KeyValueStore::global(cx))
            .read_kvp(&serialization_key)
            .unwrap();
        assert_eq!(serialized_state, None);
    }

    #[gpui::test]
    async fn test_terminal_added_during_restore_is_serialized_after_restore(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let serialization_key = window_handle
            .update(cx, |multi_workspace, _, cx| {
                TerminalPanel::serialization_key(multi_workspace.workspace().read(cx)).unwrap()
            })
            .unwrap();

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.restoring = true;
                    terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Never,
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
            .await
            .unwrap();

        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        let suppressed_state = cx
            .update(|cx| KeyValueStore::global(cx))
            .read_kvp(&serialization_key)
            .unwrap();
        assert_eq!(
            suppressed_state, None,
            "serialization should stay suppressed while the panel is restoring"
        );

        let default_shell_task = window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.finish_restoration(false, window, cx)
                })
            })
            .unwrap();
        assert!(
            default_shell_task.is_none(),
            "no default terminal should spawn when a terminal already exists"
        );

        cx.executor().advance_clock(Duration::from_millis(100));
        cx.run_until_parked();
        let serialized_state = cx
            .update(|cx| KeyValueStore::global(cx))
            .read_kvp(&serialization_key)
            .unwrap();
        assert!(
            serialized_state.is_some(),
            "terminal added during restore must be serialized once restoration finishes"
        );
    }

    #[gpui::test]
    async fn test_legacy_serialized_restore_keeps_interim_terminal_active(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Never,
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
            .await
            .unwrap();
        let interim_item_id = terminal_panel.read_with(cx, |terminal_panel, cx| {
            let pane = terminal_panel.active_pane.read(cx);
            assert_eq!(pane.items_len(), 1);
            pane.active_item().unwrap().item_id()
        });

        let restored_items = window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let project = workspace.read(cx).project().clone();
                deserialize_terminal_panel(
                    workspace.downgrade(),
                    project,
                    WorkspaceId::default(),
                    SerializedTerminalPanel {
                        items: SerializedItems::NoSplits(vec![12345]),
                        active_item_id: Some(12345),
                        terminal_list_width: None,
                    },
                    terminal_panel.downgrade(),
                    window,
                    cx,
                )
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        assert_eq!(restored_items, 1);
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            let pane = terminal_panel.active_pane.read(cx);
            assert_eq!(pane.items_len(), 2);
            assert_eq!(
                pane.active_item().map(|item| item.item_id()),
                Some(interim_item_id),
                "interim terminal must stay active after a legacy format restore"
            );
        });
    }

    #[gpui::test]
    async fn test_split_restore_grafts_interim_pane(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Never,
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
            .await
            .unwrap();
        let interim_pane =
            terminal_panel.read_with(cx, |terminal_panel, _| terminal_panel.active_pane.clone());

        let restored_items = window_handle
            .update(cx, |multi_workspace, window, cx| {
                let workspace = multi_workspace.workspace().clone();
                let project = workspace.read(cx).project().clone();
                deserialize_terminal_panel(
                    workspace.downgrade(),
                    project,
                    WorkspaceId::default(),
                    SerializedTerminalPanel {
                        items: SerializedItems::WithSplits(SerializedPaneGroup::Pane(
                            SerializedPane {
                                active: true,
                                children: vec![12345],
                                active_item: Some(12345),
                                pinned_count: 0,
                            },
                        )),
                        active_item_id: None,
                        terminal_list_width: None,
                    },
                    terminal_panel.downgrade(),
                    window,
                    cx,
                )
            })
            .unwrap()
            .await
            .unwrap();
        cx.run_until_parked();

        assert_eq!(restored_items, 1);
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            let panes = terminal_panel.center.panes();
            assert_eq!(
                panes.len(),
                2,
                "both the restored pane and the interim pane must survive"
            );
            let interim_panes_kept = panes.iter().filter(|pane| **pane == &interim_pane).count();
            assert_eq!(
                interim_panes_kept, 1,
                "interim pane must be grafted into the restored center"
            );
            assert_eq!(interim_pane.read(cx).items_len(), 1);
            assert_ne!(
                terminal_panel.active_pane, interim_pane,
                "unfocused interim pane must not become the active pane"
            );
        });
    }

    #[gpui::test]
    async fn test_local_terminal_in_local_project(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    cx.new(|cx| TerminalPanel::new(workspace, window, cx))
                })
            })
            .unwrap();

        let result = window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    terminal_panel.add_terminal_shell(
                        true,
                        None,
                        RevealStrategy::Always,
                        window,
                        cx,
                    )
                })
            })
            .unwrap()
            .await;

        assert!(
            result.is_ok(),
            "local terminal should successfully create in local project"
        );
    }

    struct FocusOnlyModal {
        focus_handle: gpui::FocusHandle,
    }
    impl gpui::EventEmitter<gpui::DismissEvent> for FocusOnlyModal {}
    impl gpui::Focusable for FocusOnlyModal {
        fn focus_handle(&self, _: &gpui::App) -> gpui::FocusHandle {
            self.focus_handle.clone()
        }
    }
    impl Render for FocusOnlyModal {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            gpui::div().track_focus(&self.focus_handle)
        }
    }
    impl workspace::ModalView for FocusOnlyModal {}

    async fn open_center_display_terminal(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) {
        workspace
            .update_in(cx, |workspace, window, cx| {
                TerminalPanel::add_center_terminal(workspace, window, cx, |_, cx| {
                    let terminal = cx.new(|cx| {
                        terminal::TerminalBuilder::new_display_only(
                            terminal::terminal_settings::CursorShape::default(),
                            terminal::terminal_settings::AlternateScroll::On,
                            None,
                            0,
                            cx.background_executor(),
                            util::paths::PathStyle::local(),
                        )
                        .subscribe(cx)
                    });
                    gpui::Task::ready(Ok(terminal))
                })
            })
            .await
            .unwrap();
        cx.run_until_parked();
    }

    #[gpui::test]
    async fn test_center_terminal_keeps_focus_on_active_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let modal_focus_handle = workspace.update_in(cx, |workspace, window, cx| {
            let focus_handle = cx.focus_handle();
            workspace.toggle_modal(window, cx, {
                let focus_handle = focus_handle.clone();
                move |_, _| FocusOnlyModal { focus_handle }
            });
            focus_handle
        });

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(workspace.has_active_modal(window, cx));
            assert!(
                modal_focus_handle.is_focused(window),
                "the modal should hold focus before the terminal is created"
            );
        });

        open_center_display_terminal(&workspace, cx).await;

        workspace.update_in(cx, |_, window, _| {
            assert!(
                modal_focus_handle.is_focused(window),
                "a background center terminal must not steal focus from an active modal"
            );
        });
    }

    #[gpui::test]
    async fn test_center_terminal_takes_focus_without_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .unwrap();
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        open_center_display_terminal(&workspace, cx).await;

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(!workspace.has_active_modal(window, cx));
            let terminal_view = workspace
                .active_pane()
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
                .expect("the new center terminal should be the active item");
            assert!(
                terminal_view.focus_handle(cx).contains_focused(window, cx),
                "with no modal open, a new center terminal should take focus"
            );
        });
    }

    #[gpui::test]
    async fn test_panel_terminal_keeps_focus_on_active_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .expect("Failed to read workspace");
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let modal_focus_handle = workspace.update_in(cx, |workspace, window, cx| {
            let focus_handle = cx.focus_handle();
            workspace.toggle_modal(window, cx, {
                let focus_handle = focus_handle.clone();
                move |_, _| FocusOnlyModal { focus_handle }
            });
            focus_handle
        });

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
            })
            .await
            .expect("Failed to spawn a panel terminal");
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(
                workspace.has_active_modal(window, cx),
                "a panel terminal that finishes spawning must not dismiss an active modal"
            );
            assert!(
                modal_focus_handle.is_focused(window),
                "a panel terminal that finishes spawning must not steal focus from an active modal"
            );
        });
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                terminal_panel.active_pane.read(cx).items_len(),
                1,
                "the terminal should still be added to the panel"
            );
        });
    }

    #[gpui::test]
    async fn test_panel_terminal_takes_focus_without_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
            })
            .await
            .expect("Failed to spawn a panel terminal");
        cx.run_until_parked();

        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
            let terminal_view = terminal_panel
                .active_pane
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
                .expect("the new terminal should be the active panel item");
            assert!(
                terminal_view.focus_handle(cx).contains_focused(window, cx),
                "with no modal open, a new panel terminal should take focus"
            );
        });
    }

    #[gpui::test]
    async fn test_task_terminal_keeps_focus_on_active_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .expect("Failed to read workspace");
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let modal_focus_handle = workspace.update_in(cx, |workspace, window, cx| {
            let focus_handle = cx.focus_handle();
            workspace.toggle_modal(window, cx, {
                let focus_handle = focus_handle.clone();
                move |_, _| FocusOnlyModal { focus_handle }
            });
            focus_handle
        });

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_task(echo_task(), RevealStrategy::Always, window, cx)
            })
            .await
            .expect("Failed to spawn a task terminal");
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(
                workspace.has_active_modal(window, cx),
                "a task terminal that finishes spawning must not dismiss an active modal"
            );
            assert!(
                modal_focus_handle.is_focused(window),
                "a task terminal that finishes spawning must not steal focus from an active modal"
            );
        });
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            assert_eq!(
                terminal_panel.active_pane.read(cx).items_len(),
                1,
                "the task terminal should still be added to the panel"
            );
        });
    }

    #[gpui::test]
    async fn test_task_terminal_takes_focus_without_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_task(echo_task(), RevealStrategy::Always, window, cx)
            })
            .await
            .expect("Failed to spawn a task terminal");
        cx.run_until_parked();

        terminal_panel.update_in(cx, |terminal_panel, window, cx| {
            let terminal_view = terminal_panel
                .active_pane
                .read(cx)
                .active_item()
                .and_then(|item| item.downcast::<TerminalView>())
                .expect("the new task terminal should be the active panel item");
            assert!(
                terminal_view.focus_handle(cx).contains_focused(window, cx),
                "with no modal open, a new task terminal should take focus"
            );
        });
    }

    #[gpui::test]
    async fn test_finished_restoration_keeps_focus_on_active_modal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let workspace = window_handle
            .update(cx, |multi_workspace, _, _| {
                multi_workspace.workspace().clone()
            })
            .expect("Failed to read workspace");
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        terminal_panel
            .update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_shell(false, None, RevealStrategy::Never, window, cx)
            })
            .await
            .expect("Failed to spawn a panel terminal");
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.open_panel::<TerminalPanel>(window, cx);
        });
        cx.run_until_parked();

        let modal_focus_handle = workspace.update_in(cx, |workspace, window, cx| {
            let focus_handle = cx.focus_handle();
            workspace.toggle_modal(window, cx, {
                let focus_handle = focus_handle.clone();
                move |_, _| FocusOnlyModal { focus_handle }
            });
            focus_handle
        });
        workspace.update_in(cx, |workspace, window, cx| {
            assert!(
                workspace.active_item(cx).is_none()
                    && workspace
                        .is_dock_at_position_open(terminal_panel.read(cx).position(window, cx), cx),
                "the restoration focus conditions should hold, otherwise this test is vacuous"
            );
        });

        window_handle
            .update(cx, |_, window, cx| {
                let workspace = workspace.downgrade();
                let terminal_panel = terminal_panel.downgrade();
                window.spawn(cx, async move |cx| {
                    TerminalPanel::restore_serialized_state(workspace, terminal_panel, cx).await
                })
            })
            .expect("Failed to restore serialized state")
            .await
            .expect("Failed to restore serialized state");
        cx.run_until_parked();

        workspace.update_in(cx, |workspace, window, cx| {
            assert!(
                workspace.has_active_modal(window, cx),
                "finishing restoration must not dismiss an active modal"
            );
            assert!(
                modal_focus_handle.is_focused(window),
                "finishing restoration must not steal focus from an active modal"
            );
        });
    }

    #[gpui::test]
    async fn test_inline_assist_tooltip_shows_keybinding_of_active_terminal(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        cx.update(|cx| {
            cx.bind_keys([gpui::KeyBinding::new(
                "ctrl-enter",
                InlineAssist::default(),
                Some("Terminal"),
            )])
        });

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        terminal_panel.update(cx, |panel, cx| panel.set_assistant_enabled(true, cx));
        terminal_panel
            .update_in(cx, |panel, window, cx| {
                panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();

        let button_bounds = cx
            .debug_bounds("ICON-ZedAssistant")
            .expect("inline assist button should be rendered in the terminal tab bar");
        cx.simulate_mouse_move(button_bounds.center(), None, Modifiers::default());

        cx.executor().advance_clock(Duration::from_millis(600));
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("KEY_BINDING-enter").is_some(),
            "tooltip should show the InlineAssist keybinding resolved in the terminal's context"
        );
    }

    // On Windows `echo` is a shell builtin rather than an executable, so spawning it directly fails.
    fn echo_task() -> SpawnInTerminal {
        let (command, args) = if cfg!(windows) {
            ("cmd.exe", vec!["/C".to_owned(), "echo".to_owned()])
        } else {
            ("echo", Vec::new())
        };
        SpawnInTerminal {
            command: Some(command.to_owned()),
            args,
            ..SpawnInTerminal::default()
        }
    }

    async fn init_workspace_with_panel(
        cx: &mut TestAppContext,
    ) -> (gpui::WindowHandle<MultiWorkspace>, Entity<TerminalPanel>) {
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let window_handle =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));

        let terminal_panel = window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    let panel = cx.new(|cx| TerminalPanel::new(workspace, window, cx));
                    workspace.add_panel(panel.clone(), window, cx);
                    panel
                })
            })
            .expect("Failed to initialize workspace with terminal panel");

        (window_handle, terminal_panel)
    }

    #[gpui::test]
    async fn test_terminal_panel_starts_open_follows_setting(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    assert!(
                        !terminal_panel.starts_open(window, cx),
                        "terminal panel should not start open by default"
                    );
                });
            })
            .expect("Failed to read terminal panel starts_open default");

        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().starts_open = Some(true);
            });
        });

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |terminal_panel, cx| {
                    assert!(
                        terminal_panel.starts_open(window, cx),
                        "terminal panel should start open when configured"
                    );
                });
            })
            .expect("Failed to read configured terminal panel starts_open");
    }

    #[gpui::test]
    async fn test_vertical_terminal_list_activates_clicked_terminal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        for _ in 0..2 {
            terminal_panel
                .update_in(cx, |terminal_panel, window, cx| {
                    terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Always,
                        window,
                        cx,
                    )
                })
                .await
                .expect("Failed to spawn a panel terminal");
        }
        cx.run_until_parked();

        assert!(
            cx.debug_bounds("terminal-list-item-0").is_none(),
            "the vertical terminal list should not be rendered with the default tab bar"
        );

        let first_terminal_id = terminal_panel.read_with(cx, |terminal_panel, cx| {
            let pane = terminal_panel.active_pane.read(cx);
            assert_eq!(pane.items_len(), 2);
            assert_eq!(
                pane.active_item_index(),
                1,
                "the most recently added terminal should be active"
            );
            pane.item_for_index(0)
                .expect("the first terminal should exist")
                .item_id()
        });

        cx.update(|window, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.terminal.get_or_insert_default().tabs =
                        Some(TerminalTabsLocation::Right);
                });
            });
            window.refresh();
        });
        cx.run_until_parked();

        let first_row_bounds = cx
            .debug_bounds("terminal-list-item-0")
            .expect("the vertical terminal list should be rendered");
        cx.simulate_click(first_row_bounds.center(), Modifiers::none());
        cx.run_until_parked();

        terminal_panel.read_with(cx, |terminal_panel, cx| {
            let active_item = terminal_panel
                .active_pane
                .read(cx)
                .active_item()
                .expect("the panel should have an active terminal");
            assert_eq!(
                active_item.item_id(),
                first_terminal_id,
                "clicking a row in the vertical terminal list should activate its terminal"
            );
        });
    }

    async fn init_vertical_terminal_list(
        terminal_count: usize,
        cx: &mut TestAppContext,
    ) -> (VisualTestContext, Entity<TerminalPanel>) {
        cx.executor().allow_parking();
        init_test(cx);
        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().tabs = Some(TerminalTabsLocation::Right);
            });
        });

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let mut cx = VisualTestContext::from_window(window_handle.into(), cx);
        for _ in 0..terminal_count {
            terminal_panel
                .update_in(&mut cx, |terminal_panel, window, cx| {
                    terminal_panel.add_terminal_shell(
                        false,
                        None,
                        RevealStrategy::Always,
                        window,
                        cx,
                    )
                })
                .await
                .expect("Failed to spawn a panel terminal");
        }
        cx.run_until_parked();
        (cx, terminal_panel)
    }

    fn simulate_drag(cx: &mut VisualTestContext, from: Point<Pixels>, to: Point<Pixels>) {
        cx.simulate_mouse_down(from, MouseButton::Left, Modifiers::none());
        let midpoint = point((from.x + to.x) / 2., (from.y + to.y) / 2.);
        cx.simulate_mouse_move(midpoint, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_move(to, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(to, MouseButton::Left, Modifiers::none());
        cx.run_until_parked();
    }

    fn terminal_ids(
        terminal_panel: &Entity<TerminalPanel>,
        cx: &VisualTestContext,
    ) -> Vec<EntityId> {
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            terminal_panel
                .active_pane
                .read(cx)
                .items()
                .map(|item| item.item_id())
                .collect()
        })
    }

    #[gpui::test]
    async fn test_vertical_terminal_list_drag_to_reorder(cx: &mut TestAppContext) {
        let (mut cx, terminal_panel) = init_vertical_terminal_list(3, cx).await;
        let cx = &mut cx;

        let ids_before = terminal_ids(&terminal_panel, cx);
        assert_eq!(ids_before.len(), 3);

        let last_row = cx
            .debug_bounds("terminal-list-item-2")
            .expect("the third terminal should have a row");
        let first_row = cx
            .debug_bounds("terminal-list-item-0")
            .expect("the first terminal should have a row");
        simulate_drag(cx, last_row.center(), first_row.center());

        assert_eq!(
            terminal_ids(&terminal_panel, cx),
            vec![ids_before[2], ids_before[0], ids_before[1]],
            "dragging the last row onto the first should move that terminal to the top"
        );
    }

    fn terminal_view_at(
        terminal_panel: &Entity<TerminalPanel>,
        index: usize,
        cx: &VisualTestContext,
    ) -> Entity<TerminalView> {
        terminal_panel.read_with(cx, |terminal_panel, cx| {
            terminal_panel
                .active_pane
                .read(cx)
                .item_for_index(index)
                .and_then(|item| item.downcast::<TerminalView>())
                .expect("the terminal should exist")
        })
    }

    #[gpui::test]
    async fn test_vertical_terminal_list_double_click_renames(cx: &mut TestAppContext) {
        let (mut cx, terminal_panel) = init_vertical_terminal_list(2, cx).await;
        let cx = &mut cx;
        let first_terminal = terminal_view_at(&terminal_panel, 0, cx);
        // A short title is what made the rename editor too narrow, since the editor is laid
        // over the title.
        first_terminal.update(cx, |terminal_view, cx| {
            terminal_view.set_custom_title(Some("ab".to_string()), cx);
        });
        cx.run_until_parked();

        let first_row = cx
            .debug_bounds("terminal-list-item-0")
            .expect("the first terminal should have a row")
            .center();
        cx.simulate_click(first_row, Modifiers::none());
        cx.simulate_event(MouseDownEvent {
            position: first_row,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            position: first_row,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 2,
        });
        cx.run_until_parked();

        assert!(
            first_terminal.read_with(cx, |terminal_view, _| terminal_view.is_renaming()),
            "double-clicking a row should start renaming its terminal"
        );

        let row_bounds = cx
            .debug_bounds("terminal-list-item-0")
            .expect("the first terminal should have a row");
        let editor_bounds = cx
            .debug_bounds("terminal-rename-editor")
            .expect("the rename editor should be rendered in the row");
        assert!(
            editor_bounds.size.width > row_bounds.size.width / 2.,
            "the rename editor should stretch across the row, but it is {:?} wide in a {:?} row",
            editor_bounds.size.width,
            row_bounds.size.width,
        );
    }

    #[gpui::test]
    async fn test_vertical_terminal_list_context_menu_renames(cx: &mut TestAppContext) {
        let (mut cx, terminal_panel) = init_vertical_terminal_list(2, cx).await;
        let cx = &mut cx;
        let first_terminal = terminal_view_at(&terminal_panel, 0, cx);

        let first_row = cx
            .debug_bounds("terminal-list-item-0")
            .expect("the first terminal should have a row")
            .center();
        cx.simulate_mouse_down(first_row, MouseButton::Right, Modifiers::none());
        cx.simulate_mouse_up(first_row, MouseButton::Right, Modifiers::none());
        cx.run_until_parked();

        let rename_entry = cx
            .debug_bounds("MENU_ITEM-Rename")
            .expect("right-clicking a row should open a menu with a Rename entry")
            .center();
        cx.simulate_click(rename_entry, Modifiers::none());
        cx.run_until_parked();

        assert!(
            first_terminal.read_with(cx, |terminal_view, _| terminal_view.is_renaming()),
            "choosing Rename from a row's context menu should start renaming its terminal"
        );
    }

    #[gpui::test]
    async fn test_vertical_terminal_list_resize(cx: &mut TestAppContext) {
        let (mut cx, terminal_panel) = init_vertical_terminal_list(2, cx).await;
        let cx = &mut cx;
        let list_width = |cx: &VisualTestContext| {
            terminal_panel.read_with(cx, |panel, _| panel.terminal_list_width)
        };
        assert_eq!(list_width(cx), DEFAULT_TERMINAL_LIST_WIDTH);

        let handle = cx
            .debug_bounds("terminal-list-resize-handle")
            .expect("the terminal list should have a resize handle")
            .center();
        simulate_drag(cx, handle, point(handle.x - px(50.), handle.y));
        assert_eq!(
            list_width(cx),
            DEFAULT_TERMINAL_LIST_WIDTH + px(50.),
            "dragging the handle of a right-side list to the left should widen it"
        );

        let handle = cx
            .debug_bounds("terminal-list-resize-handle")
            .expect("the terminal list should have a resize handle")
            .center();
        simulate_drag(cx, handle, point(handle.x + px(1000.), handle.y));
        assert_eq!(
            list_width(cx),
            MIN_TERMINAL_LIST_WIDTH,
            "the terminal list should not shrink below its minimum width"
        );

        let handle = cx
            .debug_bounds("terminal-list-resize-handle")
            .expect("the terminal list should have a resize handle")
            .center();
        cx.simulate_event(MouseDownEvent {
            position: handle,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 2,
            first_mouse: false,
        });
        cx.simulate_event(MouseUpEvent {
            position: handle,
            modifiers: Modifiers::none(),
            button: MouseButton::Left,
            click_count: 2,
        });
        cx.run_until_parked();
        assert_eq!(
            list_width(cx),
            DEFAULT_TERMINAL_LIST_WIDTH,
            "double-clicking the handle should reset the terminal list width"
        );
    }

    #[gpui::test]
    async fn test_vertical_terminal_list_hidden_with_single_terminal(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);
        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.terminal.get_or_insert_default().tabs = Some(TerminalTabsLocation::Left);
            });
        });

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;
        let cx = &mut VisualTestContext::from_window(window_handle.into(), cx);

        let add_terminal = |cx: &mut VisualTestContext| {
            terminal_panel.update_in(cx, |terminal_panel, window, cx| {
                terminal_panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
            })
        };

        add_terminal(cx)
            .await
            .expect("Failed to spawn a panel terminal");
        cx.run_until_parked();

        assert!(
            !terminal_panel.read_with(cx, |terminal_panel, _| terminal_panel.terminal_list_visible),
            "with a single terminal the panel should fall back to the tab bar"
        );
        assert!(
            cx.debug_bounds("terminal-list-item-0").is_none(),
            "the vertical terminal list should be hidden with a single terminal"
        );

        add_terminal(cx)
            .await
            .expect("Failed to spawn a panel terminal");
        cx.run_until_parked();

        assert!(
            terminal_panel.read_with(cx, |terminal_panel, _| terminal_panel.terminal_list_visible),
            "with two terminals the panel should show the vertical list instead of the tab bar"
        );
        assert!(
            cx.debug_bounds("terminal-list-item-1").is_some(),
            "the vertical terminal list should show a row for each terminal"
        );
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_panel_by_default(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        let panel_items_before =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());
        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");

        cx.run_until_parked();

        let panel_items_after =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());
        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        assert_eq!(
            panel_items_after,
            panel_items_before + 1,
            "Terminal should be added to the panel when no center terminal is focused"
        );
        assert_eq!(
            center_items_after, center_items_before,
            "Center pane should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_center_when_center_terminal_focused(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create center terminal");
        cx.run_until_parked();

        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        assert_eq!(center_items_before, 1, "Center pane should have 1 terminal");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    let active_item = workspace
                        .active_pane()
                        .read(cx)
                        .active_item()
                        .expect("Center pane should have an active item");
                    let terminal_view = active_item
                        .downcast::<TerminalView>()
                        .expect("Active center item should be a TerminalView");
                    window.focus(&terminal_view.focus_handle(cx), cx);
                })
            })
            .expect("Failed to focus terminal view");
        cx.run_until_parked();

        let panel_items_before =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");
        cx.run_until_parked();

        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        let panel_items_after =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        assert_eq!(
            center_items_after,
            center_items_before + 1,
            "New terminal should be added to the center pane"
        );
        assert_eq!(
            panel_items_after, panel_items_before,
            "Terminal panel should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_panel_when_panel_focused(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create panel terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |_, window, cx| {
                window.focus(&terminal_panel.read(cx).focus_handle(cx), cx);
            })
            .expect("Failed to focus terminal panel");
        cx.run_until_parked();

        let panel_items_before =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");
        cx.run_until_parked();

        let panel_items_after =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());
        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        assert_eq!(
            panel_items_after,
            panel_items_before + 1,
            "New terminal should be added to the panel when panel is focused"
        );
        assert_eq!(
            center_items_after, center_items_before,
            "Center pane should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_local_terminal_opens_in_center_when_center_terminal_focused(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create center terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    let active_item = workspace
                        .active_pane()
                        .read(cx)
                        .active_item()
                        .expect("Center pane should have an active item");
                    let terminal_view = active_item
                        .downcast::<TerminalView>()
                        .expect("Active center item should be a TerminalView");
                    window.focus(&terminal_view.focus_handle(cx), cx);
                })
            })
            .expect("Failed to focus terminal view");
        cx.run_until_parked();

        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        let panel_items_before =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal { local: true },
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal with local=true");
        cx.run_until_parked();

        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");
        let panel_items_after =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());

        assert_eq!(
            center_items_after,
            center_items_before + 1,
            "New local terminal should be added to the center pane"
        );
        assert_eq!(
            panel_items_after, panel_items_before,
            "Terminal panel should not gain a new terminal"
        );
    }

    #[gpui::test]
    async fn test_new_terminal_opens_in_panel_when_panel_focused_and_center_has_terminal(
        cx: &mut TestAppContext,
    ) {
        cx.executor().allow_parking();
        init_test(cx);

        let (window_handle, terminal_panel) = init_workspace_with_panel(cx).await;

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::add_center_terminal(workspace, window, cx, |project, cx| {
                        project.create_terminal_shell(None, cx)
                    })
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create center terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |_, window, cx| {
                terminal_panel.update(cx, |panel, cx| {
                    panel.add_terminal_shell(false, None, RevealStrategy::Always, window, cx)
                })
            })
            .expect("Failed to update workspace")
            .await
            .expect("Failed to create panel terminal");
        cx.run_until_parked();

        window_handle
            .update(cx, |_, window, cx| {
                window.focus(&terminal_panel.read(cx).focus_handle(cx), cx);
            })
            .expect("Failed to focus terminal panel");
        cx.run_until_parked();

        let panel_items_before =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());
        let center_items_before = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        window_handle
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    TerminalPanel::new_terminal(
                        workspace,
                        &workspace::NewTerminal::default(),
                        window,
                        cx,
                    );
                })
            })
            .expect("Failed to dispatch new_terminal");
        cx.run_until_parked();

        let panel_items_after =
            terminal_panel.read_with(cx, |panel, cx| panel.active_pane.read(cx).items_len());
        let center_items_after = window_handle
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_pane()
                    .read(cx)
                    .items_len()
            })
            .expect("Failed to read center pane items");

        assert_eq!(
            panel_items_after,
            panel_items_before + 1,
            "New terminal should go to panel when panel is focused, even if center has a terminal"
        );
        assert_eq!(
            center_items_after, center_items_before,
            "Center pane should not gain a new terminal when panel is focused"
        );
    }

    fn set_max_tabs(cx: &mut TestAppContext, value: Option<usize>) {
        cx.update_global(|store: &mut SettingsStore, cx| {
            store.update_user_settings(cx, |settings| {
                settings.workspace.max_tabs = value.map(|v| NonZero::new(v).unwrap())
            });
        });
    }

    pub fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            crate::init(cx);
        });
    }
}
