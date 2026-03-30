use codex_core::ARCHIVED_SESSIONS_SUBDIR;
use codex_core::SESSIONS_SUBDIR;
use codex_core::parse_turn_item;
use codex_protocol::ThreadId;
use codex_protocol::items::TurnItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionMetaLine;
use serde_json::Deserializer;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fs::File;
use std::io;
use std::io::BufRead;
use std::io::BufReader;
use std::path::Path;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReverseSearchContext {
    pub(crate) thread_id: ThreadId,
    pub(crate) forked_from_id: Option<ThreadId>,
    pub(crate) rollout_path: Option<PathBuf>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReverseSearchEntry {
    pub(crate) thread_id: ThreadId,
    pub(crate) text: String,
}

#[derive(Clone, Debug)]
struct SessionInfo {
    path: PathBuf,
    forked_from_id: Option<ThreadId>,
    updated_at: SystemTime,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UserMessageSource {
    Event,
    ResponseItem,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ThreadUserMessage {
    text: String,
    source: UserMessageSource,
}

pub(crate) fn load_reverse_search_entries(
    codex_home: &Path,
    context: &ReverseSearchContext,
) -> io::Result<Vec<ReverseSearchEntry>> {
    let mut sessions = scan_sessions(codex_home)?;
    seed_active_thread_context(&mut sessions, context)?;

    let family_ids = connected_family_ids(&sessions, context);
    let mut ordered_ids = Vec::new();
    if family_ids.contains(&context.thread_id) {
        ordered_ids.push(context.thread_id);
    }

    let mut other_ids: Vec<_> = family_ids
        .into_iter()
        .filter(|thread_id| *thread_id != context.thread_id)
        .collect();
    other_ids.sort_by(|lhs, rhs| {
        let left = sessions
            .get(lhs)
            .map(|info| info.updated_at)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let right = sessions
            .get(rhs)
            .map(|info| info.updated_at)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        right
            .cmp(&left)
            .then_with(|| lhs.to_string().cmp(&rhs.to_string()))
    });
    ordered_ids.extend(other_ids);

    let mut entries = Vec::new();
    for thread_id in ordered_ids {
        let Some(session) = sessions.get(&thread_id) else {
            continue;
        };
        entries.extend(read_thread_user_messages(&session.path, thread_id)?);
    }
    Ok(entries)
}

fn scan_sessions(codex_home: &Path) -> io::Result<HashMap<ThreadId, SessionInfo>> {
    let mut rollout_paths = Vec::new();
    collect_rollout_paths(&codex_home.join(SESSIONS_SUBDIR), &mut rollout_paths)?;
    collect_rollout_paths(
        &codex_home.join(ARCHIVED_SESSIONS_SUBDIR),
        &mut rollout_paths,
    )?;

    let mut sessions = HashMap::new();
    for path in rollout_paths {
        let Ok(meta_line) = read_session_meta_line_sync(&path) else {
            continue;
        };
        let updated_at = std::fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH);
        sessions.insert(
            meta_line.meta.id,
            SessionInfo {
                path,
                forked_from_id: meta_line.meta.forked_from_id,
                updated_at,
            },
        );
    }
    Ok(sessions)
}

fn seed_active_thread_context(
    sessions: &mut HashMap<ThreadId, SessionInfo>,
    context: &ReverseSearchContext,
) -> io::Result<()> {
    if sessions.contains_key(&context.thread_id) {
        return Ok(());
    }

    let Some(path) = context.rollout_path.as_ref() else {
        return Ok(());
    };
    let updated_at = std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH);
    sessions.insert(
        context.thread_id,
        SessionInfo {
            path: path.clone(),
            forked_from_id: context.forked_from_id,
            updated_at,
        },
    );
    Ok(())
}

fn connected_family_ids(
    sessions: &HashMap<ThreadId, SessionInfo>,
    context: &ReverseSearchContext,
) -> HashSet<ThreadId> {
    let mut edges: HashMap<ThreadId, Vec<ThreadId>> = HashMap::new();
    for (thread_id, session) in sessions {
        if let Some(parent_thread_id) = session.forked_from_id {
            edges.entry(*thread_id).or_default().push(parent_thread_id);
            edges.entry(parent_thread_id).or_default().push(*thread_id);
        }
    }
    if let Some(parent_thread_id) = context.forked_from_id {
        edges
            .entry(context.thread_id)
            .or_default()
            .push(parent_thread_id);
        edges
            .entry(parent_thread_id)
            .or_default()
            .push(context.thread_id);
    }

    let mut visited = HashSet::new();
    let mut pending = vec![context.thread_id];
    while let Some(thread_id) = pending.pop() {
        if !visited.insert(thread_id) {
            continue;
        }
        if let Some(neighbors) = edges.get(&thread_id) {
            pending.extend(neighbors.iter().copied());
        }
    }

    visited.retain(|thread_id| sessions.contains_key(thread_id));
    visited
}

fn collect_rollout_paths(root: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    if !root.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            collect_rollout_paths(&path, out)?;
            continue;
        }
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            out.push(path);
        }
    }
    Ok(())
}

fn read_session_meta_line_sync(path: &Path) -> io::Result<SessionMetaLine> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(io::Error::other(format!(
                "rollout at {} is empty",
                path.display()
            )));
        }
        if line.trim().is_empty() {
            continue;
        }
        let value = Deserializer::from_str(&line)
            .into_iter::<serde_json::Value>()
            .next()
            .transpose()
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("failed to parse session metadata line"))?;
        return serde_json::from_value::<SessionMetaLine>(value).map_err(|_| {
            io::Error::other(format!(
                "rollout at {} does not start with session metadata",
                path.display()
            ))
        });
    }
}

fn read_thread_user_messages(
    path: &Path,
    thread_id: ThreadId,
) -> io::Result<Vec<ReverseSearchEntry>> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut user_messages = Vec::new();

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let rollout_line = match serde_json::from_str::<RolloutLine>(&line) {
            Ok(rollout_line) => rollout_line,
            Err(_) => continue,
        };
        let (message, source) = match rollout_line.item {
            RolloutItem::EventMsg(EventMsg::UserMessage(event)) => {
                (event.message, UserMessageSource::Event)
            }
            RolloutItem::ResponseItem(item) => match parse_turn_item(&item) {
                Some(TurnItem::UserMessage(user)) => {
                    (user.message(), UserMessageSource::ResponseItem)
                }
                _ => continue,
            },
            RolloutItem::SessionMeta(_)
            | RolloutItem::TurnContext(_)
            | RolloutItem::Compacted(_)
            | RolloutItem::EventMsg(_) => continue,
        };
        let text = message.trim();
        if text.is_empty() {
            continue;
        }

        collapse_duplicate_user_message_pair(&mut user_messages, text, source);
    }

    Ok(user_messages
        .into_iter()
        .rev()
        .map(|message| ReverseSearchEntry {
            thread_id,
            text: message.text,
        })
        .collect())
}

fn collapse_duplicate_user_message_pair(
    user_messages: &mut Vec<ThreadUserMessage>,
    text: &str,
    source: UserMessageSource,
) {
    if let Some(last) = user_messages.last_mut()
        && last.text == text
        && last.source != source
    {
        last.source = UserMessageSource::Event;
        return;
    }

    user_messages.push(ThreadUserMessage {
        text: text.to_string(),
        source,
    });
}

#[cfg(test)]
mod tests {
    use super::ReverseSearchContext;
    use super::ReverseSearchEntry;
    use super::load_reverse_search_entries;
    use codex_core::SESSIONS_SUBDIR;
    use codex_protocol::ThreadId;
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::RolloutItem;
    use codex_protocol::protocol::RolloutLine;
    use codex_protocol::protocol::SessionMeta;
    use codex_protocol::protocol::SessionMetaLine;
    use codex_protocol::protocol::SessionSource;
    use codex_protocol::protocol::UserMessageEvent;
    use pretty_assertions::assert_eq;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;

    #[test]
    fn loads_current_thread_first_then_connected_forks() {
        let tempdir = tempdir().expect("tempdir");
        let codex_home = tempdir.path();
        let sessions_root = codex_home
            .join(SESSIONS_SUBDIR)
            .join("2026")
            .join("03")
            .join("30");
        fs::create_dir_all(&sessions_root).expect("session dir");

        let root_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("thread id");
        let current_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000002").expect("thread id");
        let sibling_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000003").expect("thread id");
        let unrelated_thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000004").expect("thread id");

        write_rollout(
            &sessions_root.join(
                "rollout-2026-03-30T10-00-00-000000000Z-00000000-0000-0000-0000-000000000001.jsonl",
            ),
            root_thread_id,
            /*forked_from_id*/ None,
            &["root prompt", "root newer"],
        );
        write_rollout(
            &sessions_root.join(
                "rollout-2026-03-30T10-05-00-000000000Z-00000000-0000-0000-0000-000000000002.jsonl",
            ),
            current_thread_id,
            Some(root_thread_id),
            &["current first", "current latest"],
        );
        write_rollout(
            &sessions_root.join(
                "rollout-2026-03-30T10-10-00-000000000Z-00000000-0000-0000-0000-000000000003.jsonl",
            ),
            sibling_thread_id,
            Some(root_thread_id),
            &["sibling first", "sibling latest"],
        );
        write_rollout(
            &sessions_root.join(
                "rollout-2026-03-30T10-15-00-000000000Z-00000000-0000-0000-0000-000000000004.jsonl",
            ),
            unrelated_thread_id,
            /*forked_from_id*/ None,
            &["unrelated latest"],
        );

        let entries = load_reverse_search_entries(
            codex_home,
            &ReverseSearchContext {
                thread_id: current_thread_id,
                forked_from_id: Some(root_thread_id),
                rollout_path: None,
            },
        )
        .expect("reverse search entries");

        assert_eq!(
            &entries[..2],
            &[
                ReverseSearchEntry {
                    thread_id: current_thread_id,
                    text: "current latest".to_string(),
                },
                ReverseSearchEntry {
                    thread_id: current_thread_id,
                    text: "current first".to_string(),
                },
            ],
        );
        let sibling_then_root = [
            ReverseSearchEntry {
                thread_id: sibling_thread_id,
                text: "sibling latest".to_string(),
            },
            ReverseSearchEntry {
                thread_id: sibling_thread_id,
                text: "sibling first".to_string(),
            },
            ReverseSearchEntry {
                thread_id: root_thread_id,
                text: "root newer".to_string(),
            },
            ReverseSearchEntry {
                thread_id: root_thread_id,
                text: "root prompt".to_string(),
            },
        ];
        let root_then_sibling = [
            ReverseSearchEntry {
                thread_id: root_thread_id,
                text: "root newer".to_string(),
            },
            ReverseSearchEntry {
                thread_id: root_thread_id,
                text: "root prompt".to_string(),
            },
            ReverseSearchEntry {
                thread_id: sibling_thread_id,
                text: "sibling latest".to_string(),
            },
            ReverseSearchEntry {
                thread_id: sibling_thread_id,
                text: "sibling first".to_string(),
            },
        ];
        assert!(
            entries[2..] == sibling_then_root[..] || entries[2..] == root_then_sibling[..],
            "unexpected remaining reverse-search entries: {:?}",
            &entries[2..],
        );
    }

    #[test]
    fn collapses_duplicate_response_and_event_user_messages() {
        let tempdir = tempdir().expect("tempdir");
        let codex_home = tempdir.path();
        let sessions_root = codex_home
            .join(SESSIONS_SUBDIR)
            .join("2026")
            .join("03")
            .join("30");
        fs::create_dir_all(&sessions_root).expect("session dir");

        let thread_id =
            ThreadId::from_string("00000000-0000-0000-0000-000000000001").expect("thread id");
        let rollout_path = sessions_root.join(
            "rollout-2026-03-30T10-00-00-000000000Z-00000000-0000-0000-0000-000000000001.jsonl",
        );

        let mut lines = vec![
            serde_json::to_string(&SessionMetaLine {
                meta: SessionMeta {
                    id: thread_id,
                    forked_from_id: None,
                    timestamp: "2026-03-30T10:00:00.000Z".to_string(),
                    cwd: "/workspace".into(),
                    originator: "codex_cli_rs".to_string(),
                    cli_version: "0.0.0".to_string(),
                    source: SessionSource::Cli,
                    agent_nickname: None,
                    agent_role: None,
                    agent_path: None,
                    model_provider: Some("openai".to_string()),
                    base_instructions: None,
                    dynamic_tools: None,
                    memory_mode: None,
                },
                git: None,
            })
            .expect("session meta"),
        ];
        for timestamp in ["2026-03-30T10:00:01.000Z", "2026-03-30T10:01:01.000Z"] {
            lines.push(format!(
                concat!(
                    "{{\"timestamp\":\"{timestamp}\",\"type\":\"response_item\",\"payload\":",
                    "{{\"type\":\"message\",\"role\":\"user\",\"content\":[{{\"type\":\"input_text\",",
                    "\"text\":\"same prompt\"}}]}}}}"
                ),
                timestamp = timestamp
            ));
            lines.push(
                serde_json::to_string(&RolloutLine {
                    timestamp: timestamp.to_string(),
                    item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                        message: "same prompt".to_string(),
                        images: Some(Vec::new()),
                        local_images: Vec::new(),
                        text_elements: Vec::new(),
                    })),
                })
                .expect("user message event"),
            );
        }
        fs::write(&rollout_path, lines.join("\n")).expect("write rollout");

        let entries = load_reverse_search_entries(
            codex_home,
            &ReverseSearchContext {
                thread_id,
                forked_from_id: None,
                rollout_path: None,
            },
        )
        .expect("reverse search entries");

        assert_eq!(
            entries,
            vec![
                ReverseSearchEntry {
                    thread_id,
                    text: "same prompt".to_string(),
                },
                ReverseSearchEntry {
                    thread_id,
                    text: "same prompt".to_string(),
                },
            ]
        );
    }

    fn write_rollout(
        path: &Path,
        thread_id: ThreadId,
        forked_from_id: Option<ThreadId>,
        prompts: &[&str],
    ) {
        let mut lines = Vec::new();
        lines.push(
            serde_json::to_string(&SessionMetaLine {
                meta: SessionMeta {
                    id: thread_id,
                    forked_from_id,
                    timestamp: "2026-03-30T10:00:00.000Z".to_string(),
                    cwd: "/workspace".into(),
                    originator: "codex_cli_rs".to_string(),
                    cli_version: "0.0.0".to_string(),
                    source: SessionSource::Cli,
                    agent_nickname: None,
                    agent_role: None,
                    agent_path: None,
                    model_provider: Some("openai".to_string()),
                    base_instructions: None,
                    dynamic_tools: None,
                    memory_mode: None,
                },
                git: None,
            })
            .expect("session meta"),
        );
        for prompt in prompts {
            lines.push(
                serde_json::to_string(&RolloutLine {
                    timestamp: "2026-03-30T10:00:01.000Z".to_string(),
                    item: RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                        message: (*prompt).to_string(),
                        images: Some(Vec::new()),
                        local_images: Vec::new(),
                        text_elements: Vec::new(),
                    })),
                })
                .expect("user message"),
            );
        }
        fs::write(path, lines.join("\n")).expect("write rollout");
    }
}
