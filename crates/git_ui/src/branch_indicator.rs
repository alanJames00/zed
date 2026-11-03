use git::repository::{Upstream, UpstreamTracking, UpstreamTrackingStatus};
use gpui::{Entity, Subscription};
use project::{
    Project,
    git_store::{GitStore, GitStoreEvent},
};
use settings::{Settings, SettingsStore};
use ui::{ButtonLike, Tooltip, prelude::*};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

use crate::git_panel_settings::GitPanelSettings;

/// The branch and sync buttons VS Code shows at the left of its status bar,
/// shown here only for the `vscode` Git panel layout.
pub struct GitBranchIndicator {
    project: Entity<Project>,
    _subscriptions: Vec<Subscription>,
}

impl GitBranchIndicator {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let subscriptions = vec![
            cx.subscribe(&git_store, Self::on_git_store_event),
            cx.observe_global::<SettingsStore>(|_, cx| cx.notify()),
        ];
        Self {
            project,
            _subscriptions: subscriptions,
        }
    }

    fn on_git_store_event(
        &mut self,
        _git_store: Entity<GitStore>,
        event: &GitStoreEvent,
        cx: &mut Context<Self>,
    ) {
        match event {
            GitStoreEvent::ActiveRepositoryChanged(_)
            | GitStoreEvent::RepositoryAdded
            | GitStoreEvent::RepositoryRemoved(_)
            | GitStoreEvent::RepositoryUpdated(_, _, true) => cx.notify(),
            _ => {}
        }
    }
}

impl Render for GitBranchIndicator {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !GitPanelSettings::get_global(cx).is_vscode_layout() {
            return div().into_any_element();
        }
        let project = self.project.read(cx);
        let can_push_and_pull = !project.is_via_collab();
        let Some(repository) = project.active_repository(cx) else {
            return div().into_any_element();
        };
        let repository = repository.read(cx);
        let branch = repository.branch.clone();
        // Detached HEAD has no branch name, so show the short commit instead.
        let Some(ref_label) = branch
            .as_ref()
            .map(|branch| branch.name().to_owned())
            .or_else(|| {
                repository
                    .head_commit
                    .as_ref()
                    .map(|commit| commit.sha.chars().take(7).collect())
            })
        else {
            return div().into_any_element();
        };
        let has_changes = repository.status_summary().count > 0;
        let label = if has_changes {
            format!("{ref_label}*")
        } else {
            ref_label
        };

        let sync_button = branch.filter(|_| can_push_and_pull).map(|branch| {
            let (icon, label, tooltip, action): (_, SharedString, _, Box<dyn gpui::Action>) =
                match branch.upstream {
                    Some(Upstream {
                        tracking:
                            UpstreamTracking::Tracked(UpstreamTrackingStatus { ahead, behind }),
                        ..
                    }) => (
                        IconName::ArrowCircle,
                        format!("{behind}↓ {ahead}↑").into(),
                        "Synchronize Changes",
                        Box::new(git::Sync),
                    ),
                    Some(Upstream {
                        tracking: UpstreamTracking::Gone,
                        ..
                    })
                    | None => (
                        IconName::ArrowUp,
                        "".into(),
                        "Publish Branch",
                        Box::new(git::Push),
                    ),
                };
            ButtonLike::new("git-branch-indicator-sync")
                .child(
                    h_flex()
                        .gap_0p5()
                        .child(Icon::new(icon).size(IconSize::Small).color(Color::Muted))
                        .when(!label.is_empty(), |this| {
                            this.child(Label::new(label).size(LabelSize::Small))
                        }),
                )
                .tooltip(Tooltip::text(tooltip))
                .on_click(move |_, window, cx| window.dispatch_action(action.boxed_clone(), cx))
        });

        h_flex()
            .gap_0p5()
            .child(
                ButtonLike::new("git-branch-indicator-branch")
                    .child(
                        h_flex()
                            .gap_0p5()
                            .child(
                                Icon::new(IconName::GitBranch)
                                    .size(IconSize::Small)
                                    .color(Color::Muted),
                            )
                            .child(Label::new(label).size(LabelSize::Small)),
                    )
                    .tooltip(Tooltip::text("Checkout Branch/Tag…"))
                    .on_click(|_, window, cx| {
                        window.dispatch_action(Box::new(zed_actions::git::Switch), cx)
                    }),
            )
            .children(sync_button)
            .into_any_element()
    }
}

impl StatusItemView for GitBranchIndicator {
    fn set_active_pane_item(
        &mut self,
        _: Option<&dyn ItemHandle>,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        None
    }
}
