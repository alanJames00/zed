use editor::{EditorSettings, ui_scrollbar_settings_from_raw};
use gpui::Pixels;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{
    FolderIndicator, GitPanelClickBehavior, GitPanelCommitEditor, GitPanelGroupBy, GitPanelLayout,
    GitPanelSortBy, GitPanelUntrackedChanges, IntoGpui, RegisterSetting, Settings, StatusStyle,
};
use ui::scrollbars::{ScrollbarVisibility, ShowScrollbar};
use workspace::dock::DockPosition;

#[derive(Copy, Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub struct ScrollbarSettings {
    pub show: Option<ShowScrollbar>,
}

#[derive(Debug, Clone, PartialEq, RegisterSetting)]
pub struct GitPanelSettings {
    pub button: bool,
    pub dock: DockPosition,
    pub default_width: Pixels,
    pub status_style: StatusStyle,
    pub file_icons: bool,
    pub folder_indicator: FolderIndicator,
    pub scrollbar: ScrollbarSettings,
    pub fallback_branch_name: String,
    pub sort_by: GitPanelSortBy,
    pub group_by: GitPanelGroupBy,
    pub collapse_untracked_diff: bool,
    pub tree_view: bool,
    pub diff_stats: bool,
    pub show_count_badge: bool,
    pub starts_open: bool,
    pub commit_title_max_length: usize,
    pub entry_primary_click_action: Option<GitPanelClickBehavior>,
    pub commit_editor: GitPanelCommitEditor,
    pub layout: GitPanelLayout,
    pub untracked_changes: GitPanelUntrackedChanges,
    pub always_show_repositories: bool,
}

#[derive(Default)]
pub(crate) struct GitPanelScrollbarAccessor;

impl ScrollbarVisibility for GitPanelScrollbarAccessor {
    fn visibility(&self, cx: &ui::App) -> ShowScrollbar {
        // TODO: This PR should have defined Editor's `scrollbar.axis`
        // as an Option<ScrollbarAxis>, not a ScrollbarAxes as it would allow you to
        // `.unwrap_or(EditorSettings::get_global(cx).scrollbar.show)`.
        //
        // Once this is fixed we can extend the GitPanelSettings with a `scrollbar.axis`
        // so we can show each axis based on the settings.
        //
        // We should fix this. PR: https://github.com/zed-industries/zed/pull/19495
        GitPanelSettings::get_global(cx)
            .scrollbar
            .show
            .unwrap_or_else(|| EditorSettings::get_global(cx).scrollbar.show)
    }
}

impl GitPanelSettings {
    pub fn is_vscode_layout(&self) -> bool {
        self.layout == GitPanelLayout::Vscode
    }

    /// The VS Code layout always groups by staging state, since its
    /// "Staged Changes" / "Changes" groups are what the inline row actions act on.
    pub fn effective_group_by(&self) -> GitPanelGroupBy {
        if self.is_vscode_layout() {
            GitPanelGroupBy::Staging
        } else {
            self.group_by
        }
    }

    /// VS Code opens a diff per file, whereas the other layouts scroll through
    /// the combined project diff. An explicit user setting wins in both.
    pub fn effective_entry_primary_click_action(&self) -> GitPanelClickBehavior {
        self.entry_primary_click_action
            .unwrap_or(if self.is_vscode_layout() {
                GitPanelClickBehavior::FileDiff
            } else {
                GitPanelClickBehavior::ProjectDiff
            })
    }

    pub fn effective_untracked_changes(&self) -> GitPanelUntrackedChanges {
        if self.is_vscode_layout() {
            self.untracked_changes
        } else {
            GitPanelUntrackedChanges::Mixed
        }
    }
}

impl Settings for GitPanelSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let git_panel = content.git_panel.clone().unwrap();
        Self {
            button: git_panel.button.unwrap(),
            dock: git_panel.dock.unwrap().into(),
            default_width: git_panel.default_width.unwrap().into_gpui(),
            status_style: git_panel.status_style.unwrap(),
            file_icons: git_panel.file_icons.unwrap(),
            folder_indicator: git_panel.folder_indicator.unwrap(),
            scrollbar: ScrollbarSettings {
                show: git_panel
                    .scrollbar
                    .unwrap()
                    .show
                    .map(ui_scrollbar_settings_from_raw),
            },
            fallback_branch_name: git_panel.fallback_branch_name.unwrap(),
            sort_by: git_panel.sort_by.unwrap(),
            group_by: git_panel.group_by.unwrap(),
            collapse_untracked_diff: git_panel.collapse_untracked_diff.unwrap(),
            tree_view: git_panel.tree_view.unwrap(),
            diff_stats: git_panel.diff_stats.unwrap(),
            show_count_badge: git_panel.show_count_badge.unwrap(),
            starts_open: git_panel.starts_open.unwrap(),
            commit_title_max_length: git_panel.commit_title_max_length.unwrap(),
            entry_primary_click_action: git_panel.entry_primary_click_action,
            commit_editor: git_panel.commit_editor.unwrap(),
            layout: git_panel.layout.unwrap(),
            untracked_changes: git_panel.untracked_changes.unwrap(),
            always_show_repositories: git_panel.always_show_repositories.unwrap(),
        }
    }
}
