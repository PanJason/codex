use crate::app_event::AppEvent;
use crate::app_event_sender::AppEventSender;
use crate::reverse_search::ReverseSearchContext;
use crate::reverse_search::load_reverse_search_entries;
use std::path::PathBuf;

pub(super) fn spawn_reverse_search_loader(
    app_event_tx: AppEventSender,
    codex_home: PathBuf,
    request_id: u64,
    context: ReverseSearchContext,
) {
    tokio::spawn(async move {
        let thread_id = context.thread_id;
        let result = tokio::task::spawn_blocking(move || {
            load_reverse_search_entries(codex_home.as_path(), &context)
                .map_err(|err| err.to_string())
        })
        .await
        .unwrap_or_else(|err| Err(format!("reverse search task failed: {err}")));
        app_event_tx.send(AppEvent::ReverseSearchEntriesLoaded {
            thread_id,
            request_id,
            result,
        });
    });
}
