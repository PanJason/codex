use super::chat_composer_history::HistoryEntry;
use crate::reverse_search::ReverseSearchEntry;
use codex_protocol::ThreadId;
use ratatui::style::Stylize;
use ratatui::text::Line;
use regex_lite::RegexBuilder;
use std::ops::Range;

#[derive(Debug, Default)]
pub(super) struct ReverseSearchState {
    active: Option<ActiveReverseSearch>,
}

#[derive(Debug)]
struct ActiveReverseSearch {
    thread_id: ThreadId,
    request_id: u64,
    original_draft: HistoryEntry,
    query: String,
    entries: Option<Vec<ReverseSearchEntry>>,
    filtered_matches: Vec<SearchMatch>,
    selected_match: usize,
    pending_cycle_backwards: usize,
    load_error: Option<String>,
    regex_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchMatch {
    entry_index: usize,
    range: Range<usize>,
}

impl ReverseSearchState {
    pub(super) fn is_active(&self) -> bool {
        self.active.is_some()
    }

    pub(super) fn begin(
        &mut self,
        thread_id: ThreadId,
        request_id: u64,
        original_draft: HistoryEntry,
    ) {
        self.active = Some(ActiveReverseSearch {
            thread_id,
            request_id,
            original_draft,
            query: String::new(),
            entries: None,
            filtered_matches: Vec::new(),
            selected_match: 0,
            pending_cycle_backwards: 0,
            load_error: None,
            regex_error: None,
        });
    }

    pub(super) fn original_draft(&self) -> Option<&HistoryEntry> {
        self.active.as_ref().map(|active| &active.original_draft)
    }

    pub(super) fn install_entries(
        &mut self,
        thread_id: ThreadId,
        request_id: u64,
        result: Result<Vec<ReverseSearchEntry>, String>,
    ) -> bool {
        let Some(active) = self.active.as_mut() else {
            return false;
        };
        if active.thread_id != thread_id || active.request_id != request_id {
            return false;
        }

        match result {
            Ok(entries) => {
                active.entries = Some(entries);
                active.load_error = None;
            }
            Err(err) => {
                active.entries = Some(Vec::new());
                active.load_error = Some(err);
            }
        }
        refresh_matches(active);
        true
    }

    pub(super) fn append_query_char(&mut self, ch: char) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.query.push(ch);
        refresh_matches(active);
    }

    pub(super) fn append_query_str(&mut self, query: &str) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.query.push_str(query);
        refresh_matches(active);
    }

    pub(super) fn pop_query_char(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        active.query.pop();
        refresh_matches(active);
    }

    pub(super) fn cycle_backwards(&mut self) {
        let Some(active) = self.active.as_mut() else {
            return;
        };
        if active.filtered_matches.is_empty() {
            if active.entries.is_none() {
                active.pending_cycle_backwards = active.pending_cycle_backwards.saturating_add(1);
            }
            return;
        }
        active.selected_match = (active.selected_match + 1) % active.filtered_matches.len();
    }

    pub(super) fn display_entry(&self) -> Option<HistoryEntry> {
        let active = self.active.as_ref()?;
        Some(match active.current_match() {
            Some(entry) => HistoryEntry::new(entry.text.clone()),
            None => active.original_draft.clone(),
        })
    }

    pub(super) fn current_match_range(&self) -> Option<Range<usize>> {
        self.active
            .as_ref()
            .and_then(ActiveReverseSearch::current_match_range)
    }

    pub(super) fn accept(&mut self) -> Option<HistoryEntry> {
        let active = self.active.take()?;
        Some(match active.current_match() {
            Some(entry) => HistoryEntry::new(entry.text.clone()),
            None => active.original_draft,
        })
    }

    pub(super) fn cancel(&mut self) -> Option<HistoryEntry> {
        self.active.take().map(|active| active.original_draft)
    }

    pub(super) fn footer_line(&self) -> Option<Line<'static>> {
        let active = self.active.as_ref()?;
        let mut spans = Vec::new();
        if active.is_failing() {
            spans.push("failing ".red());
        }
        spans.push("bck-i-search".cyan().bold());
        spans.push(": ".into());
        spans.push(active.query.clone().into());
        spans.push("_".dim());
        if active.entries.is_none() {
            spans.push(" (loading...)".dim());
        }
        Some(Line::from(spans))
    }
}

impl ActiveReverseSearch {
    fn current_match(&self) -> Option<&ReverseSearchEntry> {
        if self.query.is_empty() {
            return None;
        }
        let entries = self.entries.as_ref()?;
        let entry_index = self.filtered_matches.get(self.selected_match)?.entry_index;
        entries.get(entry_index)
    }

    fn current_match_range(&self) -> Option<Range<usize>> {
        self.filtered_matches
            .get(self.selected_match)
            .map(|search_match| search_match.range.clone())
    }

    fn is_failing(&self) -> bool {
        self.load_error.is_some()
            || self.regex_error.is_some()
            || (!self.query.is_empty()
                && self.entries.is_some()
                && self.filtered_matches.is_empty())
    }
}

fn refresh_matches(active: &mut ActiveReverseSearch) {
    active.selected_match = 0;
    active.regex_error = None;
    active.filtered_matches.clear();
    if active.query.is_empty() {
        return;
    }
    let Some(entries) = active.entries.as_ref() else {
        return;
    };

    let regex = match RegexBuilder::new(&active.query).build() {
        Ok(regex) => regex,
        Err(err) => {
            active.regex_error = Some(err.to_string());
            return;
        }
    };

    active.filtered_matches = entries
        .iter()
        .enumerate()
        .filter_map(|(index, entry)| {
            regex.find(&entry.text).map(|regex_match| SearchMatch {
                entry_index: index,
                range: regex_match.start()..regex_match.end(),
            })
        })
        .collect();

    if !active.filtered_matches.is_empty() {
        active.selected_match = active.pending_cycle_backwards % active.filtered_matches.len();
    }
    active.pending_cycle_backwards = 0;
}

#[cfg(test)]
mod tests {
    use super::ReverseSearchState;
    use crate::bottom_pane::chat_composer_history::HistoryEntry;
    use crate::reverse_search::ReverseSearchEntry;
    use codex_protocol::ThreadId;
    use pretty_assertions::assert_eq;

    #[test]
    fn cycles_matches_and_accepts_current_result() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("thread id");
        let mut state = ReverseSearchState::default();
        state.begin(
            thread_id,
            /*request_id*/ 7,
            HistoryEntry::new("draft".to_string()),
        );
        assert!(state.install_entries(
            thread_id,
            /*request_id*/ 7,
            Ok(vec![
                ReverseSearchEntry {
                    thread_id,
                    text: "alpha".to_string(),
                },
                ReverseSearchEntry {
                    thread_id,
                    text: "beta".to_string(),
                },
                ReverseSearchEntry {
                    thread_id,
                    text: "gamma".to_string(),
                },
            ]),
        ));
        assert_eq!(
            state.display_entry().expect("display entry"),
            HistoryEntry::new("draft".to_string())
        );
        state.append_query_char('a');
        assert_eq!(
            state.display_entry().expect("display entry"),
            HistoryEntry::new("alpha".to_string())
        );
        assert_eq!(state.current_match_range(), Some(0..1));

        state.cycle_backwards();
        assert_eq!(
            state.accept().expect("accepted entry"),
            HistoryEntry::new("beta".to_string())
        );
    }

    #[test]
    fn invalid_regex_is_failing_and_cancel_restores_original_draft() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("thread id");
        let original = HistoryEntry::new("draft".to_string());
        let mut state = ReverseSearchState::default();
        state.begin(thread_id, /*request_id*/ 2, original.clone());
        state.install_entries(
            thread_id,
            /*request_id*/ 2,
            Ok(vec![ReverseSearchEntry {
                thread_id,
                text: "hello".to_string(),
            }]),
        );

        state.append_query_char('[');

        let footer = state.footer_line().expect("footer");
        let footer_text = footer
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(footer_text.contains("failing bck-i-search"));
        assert_eq!(state.cancel(), Some(original));
    }

    #[test]
    fn empty_query_is_not_failing_and_keeps_original_draft_visible() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("thread id");
        let original = HistoryEntry::new(String::new());
        let mut state = ReverseSearchState::default();
        state.begin(thread_id, /*request_id*/ 3, original.clone());
        assert!(state.install_entries(
            thread_id,
            /*request_id*/ 3,
            Ok(vec![ReverseSearchEntry {
                thread_id,
                text: "latest history entry".to_string(),
            }]),
        ));

        let footer = state.footer_line().expect("footer");
        let footer_text = footer
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(footer_text, "bck-i-search: _");
        assert_eq!(state.display_entry(), Some(original));
        assert_eq!(state.current_match_range(), None);
    }

    #[test]
    fn queued_cycle_is_applied_after_entries_finish_loading() {
        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("thread id");
        let mut state = ReverseSearchState::default();
        state.begin(
            thread_id,
            /*request_id*/ 9,
            HistoryEntry::new("draft".to_string()),
        );
        state.append_query_char('a');
        state.cycle_backwards();

        assert!(state.install_entries(
            thread_id,
            /*request_id*/ 9,
            Ok(vec![
                ReverseSearchEntry {
                    thread_id,
                    text: "alpha".to_string(),
                },
                ReverseSearchEntry {
                    thread_id,
                    text: "beta".to_string(),
                },
                ReverseSearchEntry {
                    thread_id,
                    text: "gamma".to_string(),
                },
            ]),
        ));

        assert_eq!(
            state.display_entry().expect("display entry"),
            HistoryEntry::new("beta".to_string())
        );
    }
}
