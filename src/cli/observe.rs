//! `syscity observe` commands.
//!
//! Per-turn observability introspection: aggregate stats (Duration / Turns /
//! Calls blocks), list / show / export turn records, and prune old data.
//!
//! The CLI does not depend on a running daemon. `stats` queries the SQLite
//! metric tables (falling back to the JSON turn files when the DB is absent),
//! and `show` / `list` / `export` read the JSON files directly.

use std::collections::{HashMap, HashSet};

use clap::Subcommand;
use serde_json::json;
use tracing::{debug, warn};

use crate::agent::session_store::metrics::MetricRows;
use crate::agent::session_store::{SessionStore, StoredMessage};
use crate::error::{Result, SyscityError};
use crate::observe::aggregate::{self, ModelStats, Stats, ToolStats};
use crate::observe::record::{TurnEndState, TurnRecord};

/// Default retention window (days) for `observe prune` without `--older-than`.
const DEFAULT_RETENTION_DAYS: u32 = 30;

#[derive(Debug, Subcommand)]
pub enum ObserveCommands {
    /// Aggregate stats over turn records (Duration / Turns / Calls)
    Stats {
        /// Only today's records
        #[arg(long)]
        today: bool,
        /// Only the last N days (inclusive of today)
        #[arg(long)]
        days: Option<u32>,
        /// Only turns handled by the given agent ID
        #[arg(long)]
        agent: Option<String>,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// List turn records
    List {
        /// Filter by session ID
        #[arg(long)]
        session: Option<String>,
        /// Filter by agent ID
        #[arg(long)]
        agent: Option<String>,
        /// Only turns that ended in error or abort
        #[arg(long)]
        errors: bool,
        /// Only the last N days (inclusive of today)
        #[arg(long)]
        days: Option<u32>,
        /// Maximum number of rows to show
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show a single turn record
    Show {
        /// Turn ID
        turn_id: String,
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },
    /// Export a turn as a self-contained analysis bundle (full untruncated
    /// messages + system prompt) for feeding an AI
    Export {
        /// Turn ID
        turn_id: String,
        /// Output Markdown instead of JSON
        #[arg(long)]
        md: bool,
        /// Include the last N prior messages from the same session for context
        #[arg(long)]
        context: Option<usize>,
    },
    /// Prune old observability data (JSON files + DB metric rows)
    Prune {
        /// Delete records older than N days (default 30; "30d" also accepted)
        #[arg(long)]
        older_than: Option<String>,
    },
}

/// Run an observe subcommand.
pub async fn run_observe_command(command: &ObserveCommands) -> Result<()> {
    match command {
        ObserveCommands::Stats { today, days, agent, json } => {
            run_stats(*today, *days, agent.as_deref(), *json).await
        }
        ObserveCommands::List {
            session,
            agent,
            errors,
            days,
            limit,
        } => run_list(session.as_deref(), agent.as_deref(), *errors, *days, *limit).await,
        ObserveCommands::Show { turn_id, json } => run_show(turn_id, *json).await,
        ObserveCommands::Export { turn_id, md, context } => {
            run_export(turn_id, *md, *context).await
        }
        ObserveCommands::Prune { older_than } => run_prune(older_than.as_deref()).await,
    }
}

/// Open the shared SQLite store for CLI aggregation / pruning, or `None` when
/// the DB file is absent (daemon never ran) or unusable.
async fn open_store() -> Result<Option<SessionStore>> {
    let db_path = crate::dirs::default_memory_db();
    if !db_path.exists() {
        return Ok(None);
    }
    let url = format!("sqlite:///{}", db_path.display());
    match SessionStore::new(&url).await {
        Ok(store) => Ok(Some(store)),
        Err(e) => {
            warn!("Failed to open session store at {}: {}", db_path.display(), e);
            Ok(None)
        }
    }
}

/// Locate and parse a turn record by id across all date directories.
async fn load_turn(turn_id: &str) -> Result<Option<TurnRecord>> {
    let base = crate::dirs::turns_dir();
    let entries = match std::fs::read_dir(&base) {
        Ok(e) => e,
        Err(_) => return Ok(None),
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        let file = dir.join(format!("{}.json", turn_id));
        if !file.exists() {
            continue;
        }
        let content = tokio::fs::read_to_string(&file).await.map_err(|e| {
            SyscityError::Internal(format!("Failed to read {}: {}", file.display(), e))
        })?;
        return serde_json::from_str(&content).map(Some).map_err(|e| {
            SyscityError::Internal(format!("Failed to parse {}: {}", file.display(), e))
        });
    }
    Ok(None)
}

/// Parse a day count from `--older-than` ("30" or "30d").
fn parse_days(raw: &str) -> Result<u32> {
    let s = raw.strip_suffix('d').unwrap_or(raw);
    s.parse::<u32>().map_err(|_| {
        SyscityError::Validation(format!(
            "Invalid --older-than '{}': expected a day count like 30 or 30d",
            raw
        ))
    })
}

/// Truncate a string to `max` chars, keeping the head and tail.
fn truncate_mid(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(3);
    let head_len = keep / 2;
    let tail_len = keep - head_len;
    let head: String = chars[..head_len].iter().collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{}...{}", head, tail)
}

fn state_label(state: &TurnEndState) -> &'static str {
    match state {
        TurnEndState::Complete => "complete",
        TurnEndState::Error => "error",
        TurnEndState::Aborted => "aborted",
    }
}

/// Aggregate the Duration / Turns / Calls blocks from raw DB metric rows.
fn stats_from_rows(rows: &MetricRows) -> Stats {
    let mut stats = Stats {
        turn_count: rows.turns.len(),
        ..Default::default()
    };
    if rows.turns.is_empty() && rows.llm_calls.is_empty() && rows.tool_calls.is_empty() {
        return stats;
    }

    let mut durations: Vec<u64> = rows.turns.iter().map(|t| t.duration_ms as u64).collect();
    let mut ttfts: Vec<u64> = Vec::new();
    let mut total_rounds = 0usize;
    let mut total_tools = 0usize;
    let mut models: HashMap<String, (usize, u64, u64, u64)> = HashMap::new(); // calls, dur, p_tok, c_tok
    let mut tools: HashMap<String, (Vec<u64>, usize)> = HashMap::new(); // durations, failures

    for t in &rows.turns {
        match t.state.as_str() {
            "complete" => stats.complete += 1,
            "error" => stats.error += 1,
            "aborted" => stats.aborted += 1,
            _ => {}
        }
        if t.cache_hit {
            stats.cache_hits += 1;
        }
        if let Some(ttft) = t.ttft_ms {
            ttfts.push(ttft as u64);
        }
        total_rounds += t.llm_rounds as usize;
        total_tools += t.tool_calls as usize;
    }

    if !durations.is_empty() {
        stats.avg_duration_ms = durations.iter().sum::<u64>() / durations.len() as u64;
        stats.p50_duration_ms = aggregate::percentile(&mut durations.clone(), 50.0);
        stats.p95_duration_ms = aggregate::percentile(&mut durations, 95.0);
    }
    if !ttfts.is_empty() {
        stats.avg_ttft_ms = Some(ttfts.iter().sum::<u64>() / ttfts.len() as u64);
    }
    if !rows.turns.is_empty() {
        stats.avg_rounds = total_rounds as f64 / rows.turns.len() as f64;
        stats.avg_tools = total_tools as f64 / rows.turns.len() as f64;
    }

    stats.llm_calls = rows.llm_calls.len();
    for c in &rows.llm_calls {
        stats.prompt_tokens += c.prompt_tokens as u64;
        stats.completion_tokens += c.completion_tokens as u64;
        stats.cache_read_tokens += c.cache_read_tokens as u64;
        let entry = models.entry(c.model.clone()).or_default();
        entry.0 += 1;
        entry.1 += c.duration_ms as u64;
        entry.2 += c.prompt_tokens as u64;
        entry.3 += c.completion_tokens as u64;
    }
    if stats.prompt_tokens > 0 {
        stats.cache_hit_rate = Some(stats.cache_read_tokens as f64 / stats.prompt_tokens as f64);
    }

    stats.by_model = models
        .into_iter()
        .map(|(model, (calls, dur, p, c))| ModelStats {
            model,
            calls,
            avg_duration_ms: if calls > 0 { dur / calls as u64 } else { 0 },
            prompt_tokens: p,
            completion_tokens: c,
        })
        .collect();
    stats
        .by_model
        .sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.model.cmp(&b.model)));

    for t in &rows.tool_calls {
        let entry = tools.entry(t.name.clone()).or_default();
        entry.0.push(t.duration_ms as u64);
        if !t.success {
            entry.1 += 1;
        }
    }
    stats.by_tool = tools
        .into_iter()
        .map(|(name, (mut durs, failures))| {
            let calls = durs.len();
            ToolStats {
                name,
                calls,
                avg_duration_ms: if calls > 0 {
                    durs.iter().sum::<u64>() / calls as u64
                } else {
                    0
                },
                p95_duration_ms: aggregate::percentile(&mut durs, 95.0),
                failures,
            }
        })
        .collect();
    stats
        .by_tool
        .sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.name.cmp(&b.name)));

    stats
}

fn print_stats(stats: &Stats, json: bool) {
    if json {
        let body = json!({
            "turns": {
                "total": stats.turn_count,
                "complete": stats.complete,
                "error": stats.error,
                "aborted": stats.aborted,
                "cache_hits": stats.cache_hits,
                "avg_rounds": stats.avg_rounds,
                "avg_tools": stats.avg_tools,
            },
            "duration": {
                "avg_ms": stats.avg_duration_ms,
                "p50_ms": stats.p50_duration_ms,
                "p95_ms": stats.p95_duration_ms,
                "avg_ttft_ms": stats.avg_ttft_ms,
            },
            "calls": {
                "llm_calls": stats.llm_calls,
                "prompt_tokens": stats.prompt_tokens,
                "completion_tokens": stats.completion_tokens,
                "cache_read_tokens": stats.cache_read_tokens,
                "cache_hit_rate": stats.cache_hit_rate,
                "by_model": stats.by_model.iter().map(|m| json!({
                    "model": m.model,
                    "calls": m.calls,
                    "avg_duration_ms": m.avg_duration_ms,
                    "prompt_tokens": m.prompt_tokens,
                    "completion_tokens": m.completion_tokens,
                })).collect::<Vec<_>>(),
                "by_tool": stats.by_tool.iter().map(|t| json!({
                    "name": t.name,
                    "calls": t.calls,
                    "avg_duration_ms": t.avg_duration_ms,
                    "p95_duration_ms": t.p95_duration_ms,
                    "failures": t.failures,
                })).collect::<Vec<_>>(),
            }
        });
        println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
        return;
    }

    println!("Observability stats");
    println!("====================");
    println!();
    println!("Duration");
    println!("  turns:      {}", stats.turn_count);
    println!("  avg:        {} ms", stats.avg_duration_ms);
    println!("  p50:        {} ms", stats.p50_duration_ms);
    println!("  p95:        {} ms", stats.p95_duration_ms);
    println!(
        "  avg TTFT:   {}",
        stats
            .avg_ttft_ms
            .map(|v| format!("{} ms", v))
            .unwrap_or_else(|| "n/a".into())
    );
    println!();
    println!("Turns");
    let pct = |n: usize| {
        if stats.turn_count > 0 {
            format!(" ({:.1}%)", n as f64 * 100.0 / stats.turn_count as f64)
        } else {
            String::new()
        }
    };
    println!("  total:      {}", stats.turn_count);
    println!("  complete:   {}{}", stats.complete, pct(stats.complete));
    println!("  error:      {}{}", stats.error, pct(stats.error));
    println!("  aborted:    {}{}", stats.aborted, pct(stats.aborted));
    println!("  cache hits: {}", stats.cache_hits);
    println!("  avg rounds: {:.2}", stats.avg_rounds);
    println!("  avg tools:  {:.2}", stats.avg_tools);
    println!();
    println!("Calls");
    println!("  llm calls:          {}", stats.llm_calls);
    println!("  prompt tokens:      {}", stats.prompt_tokens);
    println!("  completion tokens:  {}", stats.completion_tokens);
    let rate = stats
        .cache_hit_rate
        .map(|r| format!("{:.1}%", r * 100.0))
        .unwrap_or_else(|| "n/a".into());
    println!("  cache hit rate:     {}", rate);
    if !stats.by_model.is_empty() {
        println!("  by model:");
        for m in &stats.by_model {
            println!(
                "    {:<24} {:>5} calls  avg {:>6} ms  {}/{} tok",
                truncate_mid(&m.model, 24),
                m.calls,
                m.avg_duration_ms,
                m.prompt_tokens,
                m.completion_tokens
            );
        }
    }
    if !stats.by_tool.is_empty() {
        println!("  by tool:");
        for t in &stats.by_tool {
            println!(
                "    {:<24} {:>5} calls  avg {:>6} ms  p95 {:>6} ms  {} failures",
                truncate_mid(&t.name, 24),
                t.calls,
                t.avg_duration_ms,
                t.p95_duration_ms,
                t.failures
            );
        }
    }
}

async fn run_stats(today: bool, days: Option<u32>, agent: Option<&str>, json: bool) -> Result<()> {
    let (since_ms, since_date) = match (today, days) {
        (true, _) => (
            Some(crate::observe::prune::cutoff_ms(0)),
            Some(crate::observe::prune::cutoff_date(0)),
        ),
        (false, Some(d)) => (
            Some(crate::observe::prune::cutoff_ms(d)),
            Some(crate::observe::prune::cutoff_date(d)),
        ),
        (false, None) => (None, None),
    };

    // Prefer the SQLite metric tables; fall back to the JSON turn files.
    if let Some(store) = open_store().await? {
        match store.load_metric_rows(since_ms, None).await {
            Ok(mut rows) => {
                if let Some(agent) = agent {
                    filter_rows_by_agent(&mut rows, agent);
                }
                let stats = stats_from_rows(&rows);
                print_stats(&stats, json);
                return Ok(());
            }
            Err(e) => {
                warn!("DB metrics query failed ({}); falling back to JSON turn files", e);
            }
        }
    }

    let (mut records, skipped) =
        aggregate::load_records(&crate::dirs::turns_dir(), since_date.as_deref());
    if skipped > 0 {
        debug!("Skipped {} unparseable turn records", skipped);
    }
    if let Some(agent) = agent {
        records.retain(|r| r.agent_id == agent);
    }
    let stats = aggregate::compute_stats(&records);
    print_stats(&stats, json);
    Ok(())
}

/// Restrict metric rows to a single agent. `turn_outcomes` carries `agent_id`;
/// `llm_calls` / `tool_call_metrics` do not, so their rows are filtered by the
/// surviving `turn_id` set.
fn filter_rows_by_agent(rows: &mut MetricRows, agent: &str) {
    let turn_ids: HashSet<String> = rows
        .turns
        .iter()
        .filter(|t| t.agent_id == agent)
        .map(|t| t.turn_id.clone())
        .collect();
    rows.turns.retain(|t| t.agent_id == agent);
    rows.llm_calls.retain(|c| turn_ids.contains(&c.turn_id));
    rows.tool_calls.retain(|c| turn_ids.contains(&c.turn_id));
}

async fn run_list(
    session: Option<&str>,
    agent: Option<&str>,
    errors: bool,
    days: Option<u32>,
    limit: usize,
) -> Result<()> {
    let since = days.map(crate::observe::prune::cutoff_date);
    let (records, skipped) = aggregate::load_records(&crate::dirs::turns_dir(), since.as_deref());
    if skipped > 0 {
        debug!("Skipped {} unparseable turn records", skipped);
    }

    let mut matches: Vec<TurnRecord> = records
        .into_iter()
        .filter(|r| match session {
            Some(s) => r.session_id.as_deref() == Some(s),
            None => true,
        })
        .filter(|r| match agent {
            Some(a) => r.agent_id == a,
            None => true,
        })
        .filter(|r| !errors || matches!(r.state, TurnEndState::Error | TurnEndState::Aborted))
        .collect();
    matches.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    let total = matches.len();
    matches.truncate(limit);

    if matches.is_empty() {
        println!("No turn records found.");
        return Ok(());
    }

    println!(
        "{:<10} {:<8} {:<36} {:<25} {:>9} {:<14} {:>4} {:>4}",
        "STATE", "AGENT", "TURN_ID", "STARTED_AT", "DURATION", "MODEL", "RNDS", "TOOLS"
    );
    println!("{}", "-".repeat(117));
    for r in &matches {
        println!(
            "{:<10} {:<8} {:<36} {:<25} {:>8}ms {:<14} {:>4} {:>4}",
            state_label(&r.state),
            truncate_mid(
                if r.agent_id.is_empty() {
                    "-"
                } else {
                    &r.agent_id
                },
                8
            ),
            truncate_mid(&r.turn_id, 36),
            r.started_at,
            r.duration_ms,
            truncate_mid(&r.model, 14),
            r.llm_rounds.len(),
            r.tool_calls.len(),
        );
    }
    println!("{}", "-".repeat(117));
    println!("{} records shown ({} matched)", matches.len(), total);
    Ok(())
}

async fn run_show(turn_id: &str, json: bool) -> Result<()> {
    let rec = match load_turn(turn_id).await? {
        Some(rec) => rec,
        None => {
            return Err(SyscityError::NotFound {
                resource: format!("turn record {}", turn_id),
            });
        }
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&rec).unwrap_or_default());
    } else {
        print_turn_human(&rec);
    }
    Ok(())
}

fn print_turn_human(rec: &TurnRecord) {
    println!("Turn {} [{}]", rec.turn_id, state_label(&rec.state));
    println!("  started:    {}", rec.started_at);
    println!("  finished:   {}", rec.finished_at);
    println!("  duration:   {} ms", rec.duration_ms);
    println!(
        "  ttft:       {}",
        rec.ttft_ms
            .map(|v| format!("{} ms", v))
            .unwrap_or_else(|| "n/a".into())
    );
    println!(
        "  queue wait: {}",
        rec.queue_wait_ms
            .map(|v| format!("{} ms", v))
            .unwrap_or_else(|| "n/a".into())
    );
    println!("  model:      {}", rec.model);
    println!("  cache hit:  {}", if rec.cache_hit { "yes" } else { "no" });
    println!(
        "  usage:      {} in / {} out / {} cache-read",
        rec.usage.prompt_tokens, rec.usage.completion_tokens, rec.usage.cache_read_tokens
    );
    println!(
        "  agent:      {}",
        if rec.agent_id.is_empty() {
            "(none)"
        } else {
            &rec.agent_id
        }
    );
    println!("  session:    {}", rec.session_id.as_deref().unwrap_or("n/a"));
    println!(
        "  conversation: {}  thread: {}  index: {}",
        rec.conversation_id, rec.thread_id, rec.turn_index
    );
    if let Some(err) = &rec.error {
        let source = match err.source {
            crate::observe::record::ErrorSource::Llm => "llm",
            crate::observe::record::ErrorSource::Tool => "tool",
            crate::observe::record::ErrorSource::Internal => "internal",
        };
        println!("  error:      [{}] {}", source, err.message);
    }
    println!();
    println!("  User:       {}", rec.user_message_preview);
    println!("  Assistant:  {}", rec.assistant_text_preview);
    if !rec.reasoning_preview.is_empty() {
        println!("  Reasoning:  {}", rec.reasoning_preview);
    }
    if !rec.llm_rounds.is_empty() {
        println!();
        println!("  LLM rounds:");
        for round in &rec.llm_rounds {
            println!(
                "    round {}  {} ({})  {} ms  ttft {}  {}/{} tok  finish {} {}",
                round.round,
                round.model,
                round.provider,
                round.duration_ms,
                round
                    .ttft_ms
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "n/a".into()),
                round.usage.map(|u| u.prompt_tokens).unwrap_or(0),
                round.usage.map(|u| u.completion_tokens).unwrap_or(0),
                round.finish_reason.as_deref().unwrap_or("n/a"),
                round.error.as_deref().unwrap_or(""),
            );
        }
    }
    if !rec.tool_calls.is_empty() {
        println!();
        println!("  Tool calls:");
        for call in &rec.tool_calls {
            println!(
                "    {} (round {})  {} ms  {}",
                call.name,
                call.round,
                call.duration_ms,
                if call.success { "success" } else { "FAILED" }
            );
            if !call.args.is_empty() {
                println!("      args:   {}", call.args);
            }
            if !call.result.is_empty() {
                println!("      result: {}", call.result);
            }
        }
    }
}

async fn run_prune(older_than: Option<&str>) -> Result<()> {
    let days = match older_than {
        Some(raw) => parse_days(raw)?,
        None => DEFAULT_RETENTION_DAYS,
    };
    let cutoff = crate::observe::prune::cutoff_date(days);
    let (dirs, files) = crate::observe::prune::prune_turn_dirs(&crate::dirs::turns_dir(), &cutoff);

    let store = open_store().await?;

    let mut db_rows = (0u64, 0u64, 0u64, 0u64);
    if let Some(ref store) = store {
        let cutoff_ms = crate::observe::prune::cutoff_ms(days);
        match store.delete_metrics_before(cutoff_ms).await {
            Ok(r) => db_rows = r,
            Err(e) => warn!("Failed to prune DB metric rows: {}", e),
        }
    }

    // Attachment GC: drop CAS objects no persisted session row references
    // anymore (plus orphaned temp files from interrupted writes).
    let mut gc = None;
    if let Some(ref store) = store {
        match store.attachment_reference_texts().await {
            Ok(texts) => {
                let mut referenced = std::collections::HashSet::new();
                for text in &texts {
                    crate::attachments::extract_digests(text, &mut referenced);
                }
                match crate::attachments::gc_unreferenced(
                    &crate::dirs::attachments_dir(),
                    &referenced,
                ) {
                    Ok(outcome) => gc = Some(outcome),
                    Err(e) => warn!("Failed to sweep attachment store: {}", e),
                }
            }
            Err(e) => warn!("Failed to collect attachment references: {}", e),
        }
    }

    println!("Pruned observability data older than {} days (cutoff {}):", days, cutoff);
    println!("  JSON: {} date directories, {} turn files", dirs, files);
    println!(
        "  DB rows: {} llm_calls, {} tool_call_metrics, {} turn_outcomes, {} request_snapshots",
        db_rows.0, db_rows.1, db_rows.2, db_rows.3
    );
    if let Some(gc) = gc {
        println!(
            "  Attachments: {} files removed ({} bytes reclaimed), {} kept",
            gc.removed_files, gc.removed_bytes, gc.kept_files
        );
    }
    Ok(())
}

async fn run_export(turn_id: &str, md: bool, context: Option<usize>) -> Result<()> {
    let rec = match load_turn(turn_id).await? {
        Some(rec) => rec,
        None => {
            return Err(SyscityError::NotFound {
                resource: format!("turn record {}", turn_id),
            });
        }
    };

    // Full (untruncated) session messages for the turn, plus optional prior
    // context. Best-effort: only when a store is present and the turn carries
    // a session id.
    let store = open_store().await?;
    let mut turn_messages: Vec<StoredMessage> = Vec::new();
    let mut context_messages: Vec<StoredMessage> = Vec::new();
    if let (Some(store), Some(sid)) = (&store, &rec.session_id) {
        match store.get_session_messages_with_turns(sid).await {
            Ok(all) => {
                let (turn_msgs, ctx_msgs) = partition_messages(&all, &rec, context.unwrap_or(0));
                turn_messages = turn_msgs;
                context_messages = ctx_msgs;
            }
            Err(e) => warn!("Failed to load session messages for export: {}", e),
        }
    }

    let personality = load_personality(&rec.agent_id, &rec.conversation_id).await;
    let system_prompt = personality.as_ref().map(system_prompt_text);

    if md {
        print_export_md(&rec, &turn_messages, &context_messages, &system_prompt);
    } else {
        let body = json!({
            "turn": &rec,
            "messages": turn_messages,
            "context_messages": context_messages,
            "agent_id": rec.agent_id,
            "system_prompt": system_prompt,
        });
        println!("{}", serde_json::to_string_pretty(&body).unwrap_or_default());
    }
    Ok(())
}

/// Split a session's messages into those belonging to the given turn (matched
/// by `turn_index`, with a created-at-window fallback) and the `context`
/// messages that immediately precede them.
fn partition_messages(
    all: &[StoredMessage],
    rec: &TurnRecord,
    context: usize,
) -> (Vec<StoredMessage>, Vec<StoredMessage>) {
    let turn_start_ms = chrono::DateTime::parse_from_rfc3339(&rec.started_at)
        .map(|t| t.timestamp_millis())
        .unwrap_or(i64::MAX);

    let mut turn_msgs: Vec<StoredMessage> = Vec::new();
    let mut before: Vec<StoredMessage> = Vec::new();
    for m in all {
        let same_turn = m.turn_index == Some(rec.turn_index as i64)
            && (m.thread_id.is_none() || m.thread_id.as_deref() == Some(rec.thread_id.as_str()));
        if same_turn {
            turn_msgs.push(m.clone());
        } else if m.created_at.timestamp_millis() < turn_start_ms {
            before.push(m.clone());
        }
    }

    // Fallback: if no rows carried a matching turn_index (e.g. rows written
    // before the column migration), pick user/assistant rows within a minute
    // of the turn's start.
    if turn_msgs.is_empty() {
        for m in all {
            if m.role == "user" || m.role == "assistant" {
                let diff = (m.created_at.timestamp_millis() - turn_start_ms).abs();
                if diff < 60_000 {
                    turn_msgs.push(m.clone());
                }
            }
        }
    }

    if context > 0 && before.len() > context {
        before = before.split_off(before.len() - context);
    }
    (turn_msgs, before)
}

/// Best-effort load of the agent personality (system prompt source) for a
/// turn, preferring the turn's stable agent id, then the conversation id, and
/// falling back to the default agent.
async fn load_personality(
    agent_id: &str,
    conversation_id: &str,
) -> Option<crate::agent::AgentPersonality> {
    for id in [agent_id, conversation_id, "default"] {
        if id.is_empty() {
            continue;
        }
        let dir = crate::dirs::agent_dir(id);
        match crate::agent::AgentPersonality::load(&dir).await {
            Ok(p) if p.is_valid => return Some(p),
            Ok(_) | Err(_) => {}
        }
    }
    None
}

/// Compose the full system prompt text from an agent's personality files.
fn system_prompt_text(p: &crate::agent::AgentPersonality) -> String {
    let mut sections: Vec<String> = Vec::new();
    for (name, text) in [
        ("SOUL.md", &p.soul),
        ("IDENTITY.md", &p.identity),
        ("BOOTSTRAP.md", &p.bootstrap),
        ("USER.md", &p.user),
        ("AGENTS.md", &p.agents),
        ("TOOLS.md", &p.tools),
        ("HEARTBEAT.md", &p.heartbeat),
        ("MEMORY.md", &p.memory),
    ] {
        if !text.trim().is_empty() {
            sections.push(format!("# {}\n\n{}", name, text.trim()));
        }
    }
    sections.join("\n\n")
}

fn print_export_md(
    rec: &TurnRecord,
    turn_msgs: &[StoredMessage],
    ctx_msgs: &[StoredMessage],
    system_prompt: &Option<String>,
) {
    println!("# Turn export: {}", rec.turn_id);
    println!();
    println!("## Summary");
    println!();
    println!("| field | value |");
    println!("|---|---|");
    println!("| state | {} |", state_label(&rec.state));
    println!(
        "| agent | {} |",
        if rec.agent_id.is_empty() {
            "(none)"
        } else {
            &rec.agent_id
        }
    );
    println!("| started | {} |", rec.started_at);
    println!("| finished | {} |", rec.finished_at);
    println!("| duration | {} ms |", rec.duration_ms);
    println!(
        "| ttft | {} |",
        rec.ttft_ms
            .map(|v| format!("{} ms", v))
            .unwrap_or_else(|| "n/a".into())
    );
    println!("| model | {} |", rec.model);
    println!("| cache hit | {} |", rec.cache_hit);
    println!(
        "| usage | {} in / {} out / {} cache-read |",
        rec.usage.prompt_tokens, rec.usage.completion_tokens, rec.usage.cache_read_tokens
    );
    if let Some(err) = &rec.error {
        println!("| error | {} |", err.message);
    }
    println!();

    if !turn_msgs.is_empty() {
        println!("## Full messages (untruncated)");
        println!();
        for m in turn_msgs {
            println!("### {}", m.role);
            println!();
            println!("{}", m.content);
            if let Some(r) = &m.reasoning_content {
                if !r.is_empty() {
                    println!();
                    println!("<details><summary>Reasoning</summary>");
                    println!();
                    println!("{}", r);
                    println!();
                    println!("</details>");
                }
            }
            println!();
        }
    } else {
        println!("## Messages (previews)");
        println!();
        println!("**User:** {}", rec.user_message_preview);
        println!();
        println!("**Assistant:** {}", rec.assistant_text_preview);
        println!();
    }

    if !ctx_msgs.is_empty() {
        println!("## Context ({} prior messages)", ctx_msgs.len());
        println!();
        for m in ctx_msgs {
            println!("**{}:** {}", m.role, m.content);
        }
        println!();
    }

    if !rec.reasoning_preview.is_empty() {
        println!("## Reasoning preview");
        println!();
        println!("{}", rec.reasoning_preview);
        println!();
    }

    if !rec.tool_calls.is_empty() {
        println!("## Tool calls");
        println!();
        for call in &rec.tool_calls {
            println!(
                "### {} (round {}) — {}",
                call.name,
                call.round,
                if call.success { "success" } else { "FAILED" }
            );
            println!();
            println!("**args:**");
            println!("```");
            println!("{}", call.args);
            println!("```");
            println!("**result:**");
            println!("```");
            println!("{}", call.result);
            println!("```");
            println!();
        }
    }

    if !rec.llm_rounds.is_empty() {
        println!("## LLM rounds");
        println!();
        for round in &rec.llm_rounds {
            println!("### round {} — {} ({})", round.round, round.model, round.provider);
            println!();
            println!("- duration: {} ms", round.duration_ms);
            println!(
                "- ttft: {}",
                round
                    .ttft_ms
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "n/a".into())
            );
            println!(
                "- usage: {} in / {} out / {} cache-read",
                round.usage.map(|u| u.prompt_tokens).unwrap_or(0),
                round.usage.map(|u| u.completion_tokens).unwrap_or(0),
                round.usage.map(|u| u.cache_read_tokens).unwrap_or(0)
            );
            println!("- finish: {}", round.finish_reason.as_deref().unwrap_or("n/a"));
            if let Some(e) = &round.error {
                println!("- error: {}", e);
            }
            println!();
        }
    }

    println!("## Agent config / system prompt");
    println!();
    match system_prompt {
        Some(p) => println!("{}", p),
        None => println!(
            "_No agent personality files found for `{}` or `default`._",
            rec.conversation_id
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::session_store::metrics::{LlmCallRow, ToolCallMetricRow, TurnOutcomeRow};

    fn sample_rows() -> MetricRows {
        MetricRows {
            turns: vec![TurnOutcomeRow {
                turn_id: "t1".into(),
                agent_id: "worker".into(),
                state: "complete".into(),
                duration_ms: 1000,
                ttft_ms: Some(120),
                queue_wait_ms: Some(5),
                llm_rounds: 1,
                tool_calls: 1,
                cache_hit: false,
                model: "m".into(),
            }],
            llm_calls: vec![LlmCallRow {
                turn_id: "t1".into(),
                provider: "p".into(),
                model: "m".into(),
                duration_ms: 900,
                ttft_ms: Some(120),
                prompt_tokens: 100,
                completion_tokens: 50,
                cache_read_tokens: 40,
                cache_creation_tokens: 0,
                finish_reason: Some("stop".into()),
                error: None,
            }],
            tool_calls: vec![ToolCallMetricRow {
                turn_id: "t1".into(),
                name: "file_read".into(),
                duration_ms: 10,
                success: true,
            }],
        }
    }

    fn minimal_turn(turn_index: u32) -> TurnRecord {
        TurnRecord {
            schema_version: 1,
            turn_id: "t1".into(),
            session_id: Some("s1".into()),
            conversation_id: "c1".into(),
            agent_id: "worker".into(),
            thread_id: "main".into(),
            turn_index,
            state: TurnEndState::Complete,
            started_at: "2026-08-14T10:00:00+08:00".into(),
            finished_at: "2026-08-14T10:00:01+08:00".into(),
            duration_ms: 1000,
            ttft_ms: None,
            model: "m".into(),
            user_message_preview: String::new(),
            assistant_text_preview: String::new(),
            reasoning_preview: String::new(),
            queue_wait_ms: None,
            cache_hit: false,
            error: None,
            usage: crate::observe::record::ObservedUsage::default(),
            llm_rounds: vec![],
            tool_calls: vec![],
            route_log: vec![],
            budget_decisions: vec![],
            compressions: vec![],
            plan_snapshot: None,
            channel: None,
        }
    }

    fn message(role: &str, content: &str, turn_index: i64) -> StoredMessage {
        StoredMessage {
            role: role.into(),
            content: content.into(),
            reasoning_content: None,
            tool_calls_json: None,
            created_at: chrono::DateTime::parse_from_rfc3339("2026-08-14T09:00:00+08:00")
                .unwrap()
                .with_timezone(&chrono::Utc),
            thread_id: Some("main".into()),
            turn_index: Some(turn_index),
        }
    }

    #[test]
    fn stats_from_rows_aggregates_blocks() {
        let stats = stats_from_rows(&sample_rows());
        assert_eq!(stats.turn_count, 1);
        assert_eq!(stats.complete, 1);
        assert_eq!(stats.avg_duration_ms, 1000);
        assert_eq!(stats.avg_ttft_ms, Some(120));
        assert_eq!(stats.llm_calls, 1);
        assert_eq!(stats.prompt_tokens, 100);
        assert_eq!(stats.cache_read_tokens, 40);
        assert!((stats.cache_hit_rate.unwrap() - 0.4).abs() < 1e-9);
        assert_eq!(stats.by_model.len(), 1);
        assert_eq!(stats.by_tool.len(), 1);
        assert_eq!(stats.by_tool[0].calls, 1);
    }

    #[test]
    fn stats_from_rows_empty() {
        let stats = stats_from_rows(&MetricRows::default());
        assert_eq!(stats.turn_count, 0);
        assert_eq!(stats.llm_calls, 0);
    }

    #[test]
    fn filter_rows_by_agent_keeps_matching_turn_ids() {
        let mut rows = MetricRows {
            turns: vec![
                TurnOutcomeRow {
                    turn_id: "t1".into(),
                    agent_id: "worker".into(),
                    state: "complete".into(),
                    duration_ms: 1000,
                    ttft_ms: None,
                    queue_wait_ms: None,
                    llm_rounds: 1,
                    tool_calls: 1,
                    cache_hit: false,
                    model: "m".into(),
                },
                TurnOutcomeRow {
                    turn_id: "t2".into(),
                    agent_id: "main".into(),
                    state: "complete".into(),
                    duration_ms: 500,
                    ttft_ms: None,
                    queue_wait_ms: None,
                    llm_rounds: 1,
                    tool_calls: 0,
                    cache_hit: false,
                    model: "m".into(),
                },
            ],
            llm_calls: vec![
                LlmCallRow {
                    turn_id: "t1".into(),
                    provider: "p".into(),
                    model: "m".into(),
                    duration_ms: 900,
                    ttft_ms: None,
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    finish_reason: None,
                    error: None,
                },
                LlmCallRow {
                    turn_id: "t2".into(),
                    provider: "p".into(),
                    model: "m".into(),
                    duration_ms: 400,
                    ttft_ms: None,
                    prompt_tokens: 80,
                    completion_tokens: 40,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                    finish_reason: None,
                    error: None,
                },
            ],
            tool_calls: vec![ToolCallMetricRow {
                turn_id: "t1".into(),
                name: "file_read".into(),
                duration_ms: 10,
                success: true,
            }],
        };

        filter_rows_by_agent(&mut rows, "worker");
        assert_eq!(rows.turns.len(), 1);
        assert_eq!(rows.turns[0].turn_id, "t1");
        assert_eq!(rows.llm_calls.len(), 1);
        assert_eq!(rows.llm_calls[0].turn_id, "t1");
        assert_eq!(rows.tool_calls.len(), 1);
    }

    #[test]
    fn parse_days_accepts_plain_and_d_suffix() {
        assert_eq!(parse_days("30").unwrap(), 30);
        assert_eq!(parse_days("30d").unwrap(), 30);
        assert!(parse_days("abc").is_err());
    }

    #[test]
    fn truncate_mid_keeps_head_and_tail() {
        let s = "abcdefghijklmnopqrstuvwxyz";
        assert_eq!(truncate_mid(s, 30), s.to_string());
        let out = truncate_mid(s, 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.starts_with("abc"));
        assert!(out.ends_with("wxyz"));
        assert!(out.contains("..."));
    }

    #[test]
    fn partition_messages_splits_by_turn_and_context() {
        let rec = minimal_turn(2);
        let msgs = vec![
            message("user", "old-1", 0),
            message("assistant", "old-a1", 0),
            message("user", "this turn", 2),
            message("assistant", "reply", 2),
        ];
        let (turn, ctx) = partition_messages(&msgs, &rec, 2);
        assert_eq!(turn.len(), 2);
        assert_eq!(turn[0].content, "this turn");
        assert_eq!(ctx.len(), 2);
    }
}
