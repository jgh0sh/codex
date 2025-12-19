use crate::client_common::Prompt;
use crate::client_common::ResponseEvent;
use crate::codex::Session;
use crate::codex::TurnContext;
use crate::compact::content_items_to_text;
use crate::config::Config;
use crate::git_info::get_git_repo_root;
use crate::truncate::TruncationPolicy;
use crate::truncate::truncate_text;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use codex_protocol::user_input::UserInput;
use futures::StreamExt;
use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;
use std::path::PathBuf;
use tokio::fs;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tracing::warn;

pub(crate) const MEMORIES_DIRNAME: &str = ".codex";
pub(crate) const MEMORIES_FILENAME: &str = "memories.md";
pub(crate) const MEMORIES_HEADER: &str = "## Memories";
pub(crate) const MEMORIES_SEPARATOR: &str = "\n\n--- memories ---\n\n";

const MEMORIES_FILE_HEADER: &str = "# Memories";
const MEMORIES_MAX_BYTES: usize = 8 * 1024;
const MEMORIES_PROMPT: &str = include_str!("../templates/memories/prompt.md");
const MEMORIES_PROMPT_MAX_BYTES: usize = 2000;
const MAX_NEW_MEMORIES_PER_TURN: usize = 6;
const NO_MEMORIES_RESPONSE: &str = "NO_MEMORIES";

pub(crate) async fn read_memories_for_instructions(config: &Config) -> Option<String> {
    let mut entries: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let paths = memory_paths(config);
    for path in paths {
        match read_memories_file(&path).await {
            Ok(values) => {
                for entry in values {
                    let trimmed = entry.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let key = trimmed.to_ascii_lowercase();
                    if seen.insert(key) {
                        entries.push(trimmed.to_string());
                    }
                }
            }
            Err(err) => {
                warn!("Failed to read memories at {}: {err:#}", path.display());
            }
        }
    }

    build_memories_section(&entries)
}

pub(crate) async fn maybe_record_memories(
    sess: &Session,
    turn_context: &TurnContext,
    inputs: &[UserInput],
) {
    if !should_record_memories(turn_context) {
        return;
    }

    let input_texts = collect_user_input_texts(inputs);
    if input_texts.is_empty() {
        return;
    }

    let mut combined = input_texts.join("\n\n");
    if combined.len() > MEMORIES_PROMPT_MAX_BYTES {
        combined = truncate_text(
            &combined,
            TruncationPolicy::Bytes(MEMORIES_PROMPT_MAX_BYTES),
        );
    }

    let prompt = Prompt {
        input: vec![ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText { text: combined }],
        }],
        tools: Vec::new(),
        parallel_tool_calls: false,
        base_instructions_override: Some(MEMORIES_PROMPT.to_string()),
        output_schema: None,
    };

    let mut stream = match turn_context.client.clone().stream(&prompt).await {
        Ok(stream) => stream,
        Err(err) => {
            warn!("Failed to run memories extraction: {err:#}");
            return;
        }
    };

    let mut output_items: Vec<String> = Vec::new();
    let mut streamed_text = String::new();

    loop {
        let Some(event) = stream.next().await else {
            warn!("Memories extraction stream closed before completion");
            return;
        };

        match event {
            Ok(ResponseEvent::OutputItemDone(item)) => {
                if let ResponseItem::Message { content, .. } = item
                    && let Some(text) = content_items_to_text(&content)
                {
                    output_items.push(text);
                }
            }
            Ok(ResponseEvent::OutputTextDelta(delta)) => {
                streamed_text.push_str(&delta);
            }
            Ok(ResponseEvent::RateLimits(snapshot)) => {
                sess.update_rate_limits(turn_context, snapshot).await;
            }
            Ok(ResponseEvent::Completed { token_usage, .. }) => {
                sess.update_token_usage_info(turn_context, token_usage.as_ref())
                    .await;
                break;
            }
            Ok(_) => {}
            Err(err) => {
                warn!("Memories extraction failed: {err:#}");
                return;
            }
        }
    }

    let raw_output = if output_items.is_empty() {
        streamed_text
    } else {
        output_items.join("\n")
    };

    let mut candidates = parse_memory_candidates(&raw_output);
    if candidates.is_empty() {
        return;
    }
    if candidates.len() > MAX_NEW_MEMORIES_PER_TURN {
        candidates.truncate(MAX_NEW_MEMORIES_PER_TURN);
    }

    let path = memory_write_path(turn_context.client.config().as_ref(), &turn_context.cwd);
    match append_memories(&path, &candidates).await {
        Ok(_) => {}
        Err(err) => {
            warn!("Failed to write memories to {}: {err:#}", path.display());
        }
    }
}

fn should_record_memories(turn_context: &TurnContext) -> bool {
    !matches!(
        turn_context.client.get_session_source(),
        SessionSource::Exec | SessionSource::SubAgent(_)
    )
}

fn memory_paths(config: &Config) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let global = config.codex_home.join(MEMORIES_FILENAME);
    paths.push(global.clone());
    if let Some(repo_path) = repo_memories_path(&config.cwd)
        && repo_path != global
    {
        paths.push(repo_path);
    }
    paths
}

fn memory_write_path(config: &Config, cwd: &Path) -> PathBuf {
    repo_memories_path(cwd).unwrap_or_else(|| config.codex_home.join(MEMORIES_FILENAME))
}

fn repo_memories_path(cwd: &Path) -> Option<PathBuf> {
    let base = if cwd.is_dir() { cwd } else { cwd.parent()? };
    let repo_root = get_git_repo_root(base)?;
    Some(repo_root.join(MEMORIES_DIRNAME).join(MEMORIES_FILENAME))
}

async fn read_memories_file(path: &Path) -> std::io::Result<Vec<String>> {
    let data = match fs::read(path).await {
        Ok(data) => data,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err),
    };
    if data.is_empty() {
        return Ok(Vec::new());
    }

    let data = if data.len() > MEMORIES_MAX_BYTES {
        warn!(
            "Memories file {} exceeds max size ({} bytes); truncating.",
            path.display(),
            MEMORIES_MAX_BYTES,
        );
        data[data.len() - MEMORIES_MAX_BYTES..].to_vec()
    } else {
        data
    };

    let text = String::from_utf8_lossy(&data);
    Ok(parse_memories(&text))
}

fn parse_memories(text: &str) -> Vec<String> {
    let mut bullets: Vec<String> = Vec::new();
    let mut lines: Vec<String> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(entry) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            bullets.push(entry.trim().to_string());
        } else {
            lines.push(trimmed.to_string());
        }
    }

    if !bullets.is_empty() { bullets } else { lines }
}

fn parse_memory_candidates(text: &str) -> Vec<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case(NO_MEMORIES_RESPONSE) {
        return Vec::new();
    }

    let mut entries: Vec<String> = Vec::new();
    for line in trimmed.lines() {
        let line = line.trim();
        if line.is_empty() || line.eq_ignore_ascii_case(NO_MEMORIES_RESPONSE) {
            continue;
        }
        if let Some(entry) = line.strip_prefix("- ").or_else(|| line.strip_prefix("* ")) {
            entries.push(entry.trim().to_string());
        } else {
            entries.push(line.to_string());
        }
    }

    let mut deduped: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for entry in entries {
        let key = entry.to_ascii_lowercase();
        if seen.insert(key) {
            deduped.push(entry);
        }
    }
    deduped
}

fn build_memories_section(entries: &[String]) -> Option<String> {
    if entries.is_empty() {
        return None;
    }

    let mut lines: Vec<String> = Vec::with_capacity(entries.len() + 1);
    lines.push(MEMORIES_HEADER.to_string());
    for entry in entries {
        lines.push(format!("- {entry}"));
    }
    Some(lines.join("\n"))
}

fn collect_user_input_texts(inputs: &[UserInput]) -> Vec<String> {
    let mut texts = Vec::new();
    for input in inputs {
        if let UserInput::Text { text } = input
            && !text.trim().is_empty()
        {
            texts.push(text.clone());
        }
    }
    texts
}

async fn append_memories(path: &Path, entries: &[String]) -> std::io::Result<usize> {
    if entries.is_empty() {
        return Ok(0);
    }

    let existing = read_memories_file(path).await?;
    let mut seen: HashSet<String> = existing
        .iter()
        .map(String::as_str)
        .map(str::to_ascii_lowercase)
        .collect();

    let mut additions = Vec::new();
    for entry in entries {
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = trimmed.to_ascii_lowercase();
        if seen.insert(key) {
            additions.push(trimmed.to_string());
        }
    }

    if additions.is_empty() {
        return Ok(0);
    }

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(dir).await?;

    let is_empty = match fs::metadata(path).await {
        Ok(meta) => meta.len() == 0,
        Err(err) if err.kind() == ErrorKind::NotFound => true,
        Err(err) => return Err(err),
    };

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    if is_empty {
        file.write_all(format!("{MEMORIES_FILE_HEADER}\n").as_bytes())
            .await?;
    } else {
        file.write_all(b"\n").await?;
    }

    for entry in &additions {
        file.write_all(format!("- {entry}\n").as_bytes()).await?;
    }

    Ok(additions.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn parse_memories_prefers_bullets() {
        let text = "# Memories\n- Prefer short diffs\n* Run tests\nextra line";
        let parsed = parse_memories(text);
        assert_eq!(parsed, vec!["Prefer short diffs", "Run tests"]);
    }

    #[test]
    fn parse_memories_falls_back_to_lines() {
        let text = "# Memories\nPrefer short diffs\nRun tests";
        let parsed = parse_memories(text);
        assert_eq!(parsed, vec!["Prefer short diffs", "Run tests"]);
    }

    #[test]
    fn parse_memory_candidates_skips_empty_and_sentinel() {
        let parsed = parse_memory_candidates("NO_MEMORIES");
        assert_eq!(parsed, Vec::<String>::new());
    }

    #[test]
    fn collect_user_input_texts_ignores_non_text() {
        let inputs = vec![
            UserInput::Text {
                text: "Hello".to_string(),
            },
            UserInput::Image {
                image_url: "data:image/png;base64,abc".to_string(),
            },
            UserInput::Text {
                text: "  ".to_string(),
            },
        ];
        let texts = collect_user_input_texts(&inputs);
        assert_eq!(texts, vec!["Hello".to_string()]);
    }

    #[test]
    fn build_memories_section_renders_header_and_bullets() {
        let entries = vec!["Prefer rustfmt".to_string(), "Run tests".to_string()];
        let section = build_memories_section(&entries).expect("section");
        assert_eq!(
            section,
            "## Memories\n- Prefer rustfmt\n- Run tests".to_string()
        );
    }
}
