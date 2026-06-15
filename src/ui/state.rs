use std::cell::Cell;

use ratatui::prelude::Color;

use crate::app::Config;
use crate::model::{AppSummary, Provider, Summary};
use crate::share::Variant;

use super::theme;

pub(super) struct UiState {
    pub(super) config: Config,
    pub(super) report: AppSummary,
    pub(super) tab_index: usize,
    pub(super) status: String,
    /// Scroll offset for the sections area (alt-screen TUIs have no terminal
    /// scrollback, so overflow must scroll in-app).
    pub(super) scroll: u16,
    /// Largest valid scroll offset, measured during the last draw.
    pub(super) max_scroll: Cell<u16>,
    /// Present when the share modal is open.
    pub(super) share: Option<ShareModal>,
}

pub(super) struct ShareModal {
    pub(super) selected: usize,
    pub(super) variant: Variant,
}

pub(super) const SHARE_ACTIONS: [&str; 4] = [
    "Share to X (image + caption)",
    "Copy image",
    "Copy caption (text)",
    "Save image to file",
];

impl ShareModal {
    pub(super) fn new() -> Self {
        Self {
            selected: 0,
            variant: Variant::Summary,
        }
    }
}

impl UiState {
    pub(super) fn tab_count(&self) -> usize {
        self.report.providers.len() + 1
    }

    pub(super) fn tabs(&self) -> Vec<(&'static str, Color)> {
        let mut tabs = self
            .report
            .providers
            .iter()
            .map(|summary| {
                (
                    summary.provider.label(),
                    theme::provider_color(summary.provider),
                )
            })
            .collect::<Vec<_>>();
        tabs.push((
            Provider::Combined.label(),
            theme::provider_color(Provider::Combined),
        ));
        tabs
    }

    pub(super) fn current_summary(&self) -> &Summary {
        self.report
            .providers
            .get(self.tab_index)
            .unwrap_or(&self.report.combined)
    }

    pub(super) fn next_tab(&mut self) {
        self.tab_index = (self.tab_index + 1) % self.tab_count();
        self.scroll = 0;
    }

    pub(super) fn previous_tab(&mut self) {
        self.tab_index = if self.tab_index == 0 {
            self.tab_count() - 1
        } else {
            self.tab_index - 1
        };
        self.scroll = 0;
    }
}
