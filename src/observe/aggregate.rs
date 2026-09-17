//! Aggregation over persisted turn records.
//!
//! Pure functions used by the `syscity observe` CLI to compute the
//! Duration / Turns / Calls statistics blocks. No new dependencies.

use std::collections::HashMap;
use std::path::Path;

use super::record::{TurnEndState, TurnRecord};

/// Load turn records from `<base>/<date>/<turn_id>/summary.json`, optionally
/// only those with a date directory name >= `since` (YYYY-MM-DD, lexicographic
/// compare). Returns the parsed records and the number of files skipped
/// (unparseable).
pub fn load_records(base: &Path, since: Option<&str>) -> (Vec<TurnRecord>, usize) {
    let mut records = Vec::new();
    let mut skipped = 0;

    let entries = match std::fs::read_dir(base) {
        Ok(e) => e,
        Err(_) => return (records, skipped),
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let dir_name = entry.file_name().to_string_lossy().to_string();
        // Only consider YYYY-MM-DD directories.
        if dir_name.len() != 10 || dir_name.chars().nth(4) != Some('-') {
            continue;
        }
        if let Some(since) = since {
            if dir_name.as_str() < since {
                continue;
            }
        }
        let turns = match std::fs::read_dir(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for turn in turns.flatten() {
            let tpath = turn.path();
            if !tpath.is_dir() {
                continue;
            }
            let summary = tpath.join("summary.json");
            match std::fs::read_to_string(&summary)
                .ok()
                .and_then(|c| serde_json::from_str::<TurnRecord>(&c).ok())
            {
                Some(rec) => records.push(rec),
                None => skipped += 1,
            }
        }
    }

    records.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    (records, skipped)
}

/// Nearest-rank percentile over a set of values (ms).
pub fn percentile(values: &mut [u64], p: f64) -> u64 {
    if values.is_empty() {
        return 0;
    }
    values.sort_unstable();
    let idx = ((p / 100.0) * values.len() as f64).ceil() as usize;
    let idx = idx.saturating_sub(1).min(values.len() - 1);
    values[idx]
}

#[derive(Debug, Default)]
pub struct Stats {
    // Duration
    pub turn_count: usize,
    pub avg_duration_ms: u64,
    pub p50_duration_ms: u64,
    pub p95_duration_ms: u64,
    pub avg_ttft_ms: Option<u64>,
    // Turns
    pub complete: usize,
    pub error: usize,
    pub aborted: usize,
    pub cache_hits: usize,
    pub avg_rounds: f64,
    pub avg_tools: f64,
    // Calls
    pub llm_calls: usize,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_hit_rate: Option<f64>,
    /// What the gateway's own estimate said these prompts would cost, summed
    /// over the rounds that reported both numbers.
    ///
    /// Sits next to the provider's count so the two can be reconciled: budget,
    /// pruning and cost-guard decisions are all made on the estimate, and this
    /// is how far off it was.
    pub estimated_prompt_tokens: u64,
    /// `prompt_tokens / estimated_prompt_tokens` over those same rounds —
    /// how much the estimate under- or over-counts, as a ratio.
    pub estimate_drift: Option<f64>,
    pub by_model: Vec<ModelStats>,
    pub by_tool: Vec<ToolStats>,
}

#[derive(Debug, Default)]
pub struct ModelStats {
    pub model: String,
    pub calls: usize,
    pub avg_duration_ms: u64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

#[derive(Debug, Default)]
pub struct ToolStats {
    pub name: String,
    pub calls: usize,
    pub avg_duration_ms: u64,
    pub p95_duration_ms: u64,
    pub failures: usize,
}

/// Compute aggregate statistics over a set of turn records.
pub fn compute_stats(records: &[TurnRecord]) -> Stats {
    let mut stats = Stats {
        turn_count: records.len(),
        ..Default::default()
    };
    if records.is_empty() {
        return stats;
    }

    let mut durations: Vec<u64> = Vec::with_capacity(records.len());
    let mut ttfts: Vec<u64> = Vec::new();
    let mut total_rounds = 0usize;
    let mut total_tools = 0usize;
    let mut models: HashMap<String, (usize, u64, u64, u64)> = HashMap::new(); // calls, dur_sum, p_tok, c_tok
    let mut tools: HashMap<String, (Vec<u64>, usize)> = HashMap::new(); // durations, failures

    for rec in records {
        durations.push(rec.duration_ms);
        if let Some(t) = rec.ttft_ms {
            ttfts.push(t);
        }
        match rec.state {
            TurnEndState::Complete => stats.complete += 1,
            TurnEndState::Error => stats.error += 1,
            TurnEndState::Aborted => stats.aborted += 1,
        }
        if rec.cache_hit {
            stats.cache_hits += 1;
        }
        total_rounds += rec.llm_rounds.len();
        total_tools += rec.tool_calls.len();

        for round in &rec.llm_rounds {
            stats.llm_calls += 1;
            if let Some(u) = &round.usage {
                stats.prompt_tokens += u.prompt_tokens as u64;
                stats.completion_tokens += u.completion_tokens as u64;
                stats.cache_read_tokens += u.cache_read_tokens as u64;
                // Only the rounds with both numbers say anything about the
                // estimate: one without a provider count has nothing to compare
                // against, and one without an estimate was never estimated.
                if let Some(estimated) = round.estimated_prompt_tokens {
                    stats.estimated_prompt_tokens += estimated as u64;
                }
            }
            let entry = models.entry(round.model.clone()).or_default();
            entry.0 += 1;
            entry.1 += round.duration_ms;
            if let Some(u) = &round.usage {
                entry.2 += u.prompt_tokens as u64;
                entry.3 += u.completion_tokens as u64;
            }
        }
        for call in &rec.tool_calls {
            let entry = tools.entry(call.name.clone()).or_default();
            entry.0.push(call.duration_ms);
            if !call.success {
                entry.1 += 1;
            }
        }
    }

    stats.avg_duration_ms = durations.iter().sum::<u64>() / records.len() as u64;
    stats.p50_duration_ms = percentile(&mut durations.clone(), 50.0);
    stats.p95_duration_ms = percentile(&mut durations, 95.0);
    if !ttfts.is_empty() {
        stats.avg_ttft_ms = Some(ttfts.iter().sum::<u64>() / ttfts.len() as u64);
    }
    stats.avg_rounds = total_rounds as f64 / records.len() as f64;
    stats.avg_tools = total_tools as f64 / records.len() as f64;
    if stats.prompt_tokens > 0 {
        stats.cache_hit_rate = Some(stats.cache_read_tokens as f64 / stats.prompt_tokens as f64);
    }
    if stats.estimated_prompt_tokens > 0 {
        stats.estimate_drift =
            Some(stats.prompt_tokens as f64 / stats.estimated_prompt_tokens as f64);
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
                p95_duration_ms: percentile(&mut durs, 95.0),
                failures,
            }
        })
        .collect();
    stats
        .by_tool
        .sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.name.cmp(&b.name)));

    stats
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observe::record::{LlmRoundRecord, ObservedToolCall, ObservedUsage};
    use tempfile::TempDir;

    fn rec(id: &str, duration: u64, state: TurnEndState) -> TurnRecord {
        TurnRecord {
            schema_version: 1,
            turn_id: id.into(),
            session_id: Some("s".into()),
            conversation_id: "c".into(),
            agent_id: "worker".into(),
            thread_id: "main".into(),
            turn_index: 0,
            state,
            started_at: "2026-08-14T10:00:00+08:00".into(),
            finished_at: "2026-08-14T10:00:01+08:00".into(),
            duration_ms: duration,
            ttft_ms: Some(100),
            model: "m".into(),
            user_message_preview: String::new(),
            assistant_text_preview: String::new(),
            reasoning_preview: String::new(),
            queue_wait_ms: None,
            cache_hit: false,
            error: None,
            usage: ObservedUsage::default(),
            llm_rounds: vec![LlmRoundRecord {
                round: 0,
                provider: "p".into(),
                model: "m".into(),
                started_at: "2026-08-14T10:00:00+08:00".into(),
                duration_ms: duration,
                ttft_ms: Some(100),
                usage: Some(ObservedUsage {
                    prompt_tokens: 100,
                    completion_tokens: 50,
                    total_tokens: 150,
                    cache_read_tokens: 40,
                    cache_creation_tokens: 0,
                }),
                estimated_prompt_tokens: Some(120),
                finish_reason: Some("stop".into()),
                error: None,
                input: None,
                output: None,
            }],
            tool_calls: vec![ObservedToolCall {
                round: 0,
                name: "file_read".into(),
                args: String::new(),
                result: String::new(),
                success: true,
                duration_ms: 10,
                error: None,
            }],
            route_log: vec![],
            compressions: vec![],
            plan_snapshot: None,
            channel: None,
        }
    }

    #[test]
    fn percentile_basic() {
        let mut v = vec![10u64, 20, 30, 40, 50, 60, 70, 80, 90, 100];
        assert_eq!(percentile(&mut v, 50.0), 50);
        assert_eq!(percentile(&mut vec![], 95.0), 0);
    }

    #[test]
    fn compute_stats_aggregates() {
        let records = vec![
            rec("t1", 100, TurnEndState::Complete),
            rec("t2", 300, TurnEndState::Error),
            rec("t3", 200, TurnEndState::Aborted),
        ];
        let stats = compute_stats(&records);
        assert_eq!(stats.turn_count, 3);
        assert_eq!(stats.complete, 1);
        assert_eq!(stats.error, 1);
        assert_eq!(stats.aborted, 1);
        assert_eq!(stats.avg_duration_ms, 200);
        assert_eq!(stats.llm_calls, 3);
        assert_eq!(stats.prompt_tokens, 300);
        // The estimate sits beside the provider's count so the two can be
        // reconciled: the fixtures estimate 120 against 100 actual each.
        assert_eq!(stats.estimated_prompt_tokens, 360);
        assert_eq!(stats.estimate_drift, Some(300.0 / 360.0));
        assert_eq!(stats.cache_read_tokens, 120);
        assert!((stats.cache_hit_rate.unwrap() - 0.4).abs() < 1e-9);
        assert_eq!(stats.by_tool.len(), 1);
        assert_eq!(stats.by_tool[0].calls, 3);
    }

    #[test]
    fn load_records_skips_bad_files_and_filters_date() {
        let dir = TempDir::new().unwrap();
        let base = dir.path();
        let day1 = base.join("2026-08-01");
        let day2 = base.join("2026-08-14");
        std::fs::create_dir_all(day1.join("a")).unwrap();
        std::fs::create_dir_all(day2.join("b")).unwrap();
        std::fs::write(
            day1.join("a").join("summary.json"),
            serde_json::to_string(&rec("a", 1, TurnEndState::Complete)).unwrap(),
        )
        .unwrap();
        std::fs::write(
            day2.join("b").join("summary.json"),
            serde_json::to_string(&rec("b", 2, TurnEndState::Complete)).unwrap(),
        )
        .unwrap();
        // A turn dir with a corrupt summary.json is counted as skipped.
        std::fs::create_dir_all(day2.join("bad")).unwrap();
        std::fs::write(day2.join("bad").join("summary.json"), "not json").unwrap();
        std::fs::create_dir_all(base.join("not-a-date")).unwrap();

        let (all, skipped) = load_records(base, None);
        assert_eq!(all.len(), 2);
        assert_eq!(skipped, 1);

        let (recent, _) = load_records(base, Some("2026-08-10"));
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].turn_id, "b");
    }
}
