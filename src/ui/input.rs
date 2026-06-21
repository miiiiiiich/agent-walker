use crossterm::event::{KeyCode, KeyModifiers};

use crate::app::load_report;
use crate::share::ShareCard;

use super::state::{SHARE_ACTIONS, ShareModal, UiState};

pub(super) fn handle_key(state: &mut UiState, code: KeyCode, modifiers: KeyModifiers) -> bool {
    if modifiers.contains(KeyModifiers::CONTROL) && code == KeyCode::Char('c') {
        return true;
    }
    if state.share.is_some() {
        handle_share_key(state, code);
        return false;
    }
    match code {
        KeyCode::Esc | KeyCode::Char('q') => return true,
        KeyCode::Char('s') => state.share = Some(ShareModal::new()),
        KeyCode::Tab | KeyCode::Right | KeyCode::Char('l') => state.next_tab(),
        KeyCode::BackTab | KeyCode::Left | KeyCode::Char('h') => state.previous_tab(),
        KeyCode::Down | KeyCode::Char('j') => {
            state.scroll = state.scroll.saturating_add(1).min(state.max_scroll.get());
        }
        KeyCode::Up | KeyCode::Char('k') => {
            state.scroll = state.scroll.saturating_sub(1);
        }
        KeyCode::PageDown => {
            state.scroll = state.scroll.saturating_add(8).min(state.max_scroll.get());
        }
        KeyCode::PageUp => {
            state.scroll = state.scroll.saturating_sub(8);
        }
        KeyCode::Char(digit @ '1'..='9') => {
            let index = digit as usize - '1' as usize;
            if index < state.tab_count() {
                state.tab_index = index;
                state.scroll = 0;
            }
        }
        KeyCode::Char('r') => match load_report(&state.config) {
            Ok(report) => {
                state.report = report;
                state.tab_index = state.tab_index.min(state.tab_count() - 1);
                state.status = String::new();
            }
            Err(error) => {
                state.status = format!("reload failed: {error:#}");
            }
        },
        _ => {}
    }
    false
}

fn handle_share_key(state: &mut UiState, code: KeyCode) {
    let Some(modal) = state.share.as_mut() else {
        return;
    };
    match code {
        KeyCode::Esc | KeyCode::Char('q' | 's') => {
            state.share = None;
        }
        KeyCode::Down | KeyCode::Char('j') => {
            modal.selected = (modal.selected + 1) % SHARE_ACTIONS.len();
        }
        KeyCode::Up | KeyCode::Char('k') => {
            modal.selected = (modal.selected + SHARE_ACTIONS.len() - 1) % SHARE_ACTIONS.len();
        }
        KeyCode::Enter => {
            let status = execute_share(state);
            state.status = status;
            state.share = None;
        }
        _ => {}
    }
}

fn execute_share(state: &UiState) -> String {
    let Some(modal) = state.share.as_ref() else {
        return String::new();
    };
    let card = ShareCard::from_summary(state.current_summary());
    let result = if modal.selected == 0 {
        crate::share::copy_image(&card).map(|()| "card image copied to clipboard".to_owned())
    } else {
        let path = crate::share::default_save_path();
        crate::share::save(&card, &path).map(|()| format!("saved card to {}", path.display()))
    };
    result.unwrap_or_else(|error| format!("share failed: {error:#}"))
}
