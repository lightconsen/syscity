//! Post-turn quality monitoring: risk scanning, badcase collection, and turn sampling.
//! (Split out of the former single-file `agent_engine.rs`; same `impl Agent`.)

use std::sync::Arc;

use crate::agent::reflection::critic::Critic;
use crate::agent::reflection::types::{Critique, QualityCriteria};
use tracing::{info, warn};

use super::super::*;

impl Agent {
    /// Post-turn online risk scan: when a completed turn trips a risk signal,
    /// insert it into the pending-badcase pool (source `online:risk`). Runs
    /// fire-and-forget, mirroring the retrospect hook above.
    ///
    /// §八 在线质量监控: when the risk-signal count reaches the configured
    /// `online_monitoring.llm_judge_risk_threshold`, the flagged turn is also
    /// deep-judged by an LLM [`Critic`] before it is inserted, and a compact
    /// verdict summary is appended to the badcase's `risk_signals`. The judge
    /// runs in the same fire-and-forget task; any failure is `warn!`ed and
    /// never breaks the turn.
    pub(crate) fn scan_turn_for_badcase(
        &self,
        input: &str,
        response: &str,
        tool_call_count: usize,
        turn_id: &str,
        conversation_id: &str,
        compression_risks: Vec<String>,
    ) {
        let (Some(checker), Some(store)) =
            (self.risk_checker.as_ref(), self.pending_badcase_store.as_ref())
        else {
            return;
        };
        // No persisted turn to key the badcase on — nothing to collect.
        if turn_id.is_empty() || response.is_empty() {
            return;
        }
        let record = crate::eval::RiskTurnInput {
            input: input.to_string(),
            response: response.to_string(),
            tool_call_count,
        };
        let mut risks = checker.scan_turn(&record);
        // §三 压缩质量门禁：低保留率压缩是又一类在线风险信号，与响应侧风险
        // 合并后统一进 pending 池（同样受 dedup 保护，additive，不改判）。
        risks.extend(compression_risks);
        if risks.is_empty() {
            return;
        }

        // ── §八 在线质量监控: snapshot config eagerly (no lock across await) ──
        // The config is cloned here (never held as a lock) and moved into the
        // fire-and-forget task below.
        let monitoring = self.online_monitoring.clone();
        let deep_judge = if should_deep_judge(
            risks.len(),
            monitoring.enabled,
            monitoring.llm_judge_risk_threshold,
        ) {
            Some((monitoring.llm_judge_risk_threshold.max(1), monitoring.judge_model))
        } else {
            None
        };

        let provider = self.provider.clone();
        let store = Arc::clone(store);
        let session_id = self
            .session_id
            .clone()
            .unwrap_or_else(|| conversation_id.to_string());
        let agent_id = self.agent_id.clone();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            // Counted: shutdown waits for this write before closing storage.
            let _owed = crate::agent::writes::pending().guard();
            let mut risk_signals = risks;
            // Deep LLM judge on the flagged turn. Runs before the pending insert
            // so the verdict can ride along on the badcase row.
            if let Some((threshold, judge_model)) = deep_judge {
                let mut critic = Critic::new(provider);
                if let Some(model) = judge_model {
                    critic = critic.with_model(model);
                }
                let trajectory = format!(
                    "=== TURN (high-risk) ===\nUser: {}\n\nAssistant: {}",
                    record.input, record.response
                );
                let criteria = QualityCriteria::default();
                match critic
                    .evaluate_trajectory(&trajectory, &criteria, None)
                    .await
                {
                    Ok(critique) => {
                        let summary = judge_summary(&critique);
                        info!(
                            "Online monitoring: LLM judge verdict for turn {} ({} risk signals >= threshold {}): {}",
                            turn_id, risk_signals.len(), threshold, summary
                        );
                        risk_signals.push(summary);
                    }
                    Err(e) => {
                        warn!(
                            "Online monitoring: LLM judge failed for turn {} ({} risk signals): {}",
                            turn_id,
                            risk_signals.len(),
                            e
                        );
                    }
                }
            }

            let params = crate::eval::InsertPendingParams {
                source: crate::eval::PendingSource::OnlineRisk,
                turn_id: Some(turn_id),
                session_id: Some(session_id),
                agent_id: Some(agent_id),
                input: record.input,
                response: record.response,
                risk_signals,
            };
            if let Err(e) = store.insert_pending(&params).await {
                warn!("Failed to record online risk badcase: {}", e);
            }
        });
    }
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn sample_turn(
        &self,
        input: &str,
        response: &str,
        tool_call_count: usize,
        turn_id: &str,
        conversation_id: &str,
        model: String,
        cache_hit: bool,
        total_tokens: u64,
        latency_ms: u64,
    ) {
        let Some(store) = self.sample_store.as_ref() else {
            return;
        };
        if !self.sampling.enabled {
            return;
        }
        // No persisted turn to sample on (guard-rejection path).
        if turn_id.is_empty() || response.is_empty() {
            return;
        }
        // Optional deterministic skip: hash the turn id into [0, 1) and keep
        // it when below the configured rate. `sample_rate <= 0.0` keeps all.
        let rate = self.sampling.sample_rate;
        if rate > 0.0 {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hash::hash(turn_id, &mut hasher);
            let frac = (std::hash::Hasher::finish(&hasher) % 10_000) as f64 / 10_000.0;
            if frac >= rate {
                return;
            }
        }

        // Verdict + risk signals from the attached risk checker, if any.
        let (verdict, risk_signals) = match self.risk_checker.as_ref() {
            Some(checker) => {
                let record = crate::eval::RiskTurnInput {
                    input: input.to_string(),
                    response: response.to_string(),
                    tool_call_count,
                };
                let risks = checker.scan_turn(&record);
                let verdict = if risks.is_empty() {
                    crate::eval::SampleVerdict::Pass
                } else {
                    crate::eval::SampleVerdict::Flag
                };
                (verdict, risks)
            }
            None => (crate::eval::SampleVerdict::Pass, Vec::new()),
        };

        let store = Arc::clone(store);
        let session_id = self.session_id.clone();
        let agent_id = self.agent_id.clone();
        let turn_id = turn_id.to_string();
        let input = input.to_string();
        let response = response.to_string();
        let conversation_id = conversation_id.to_string();
        tokio::spawn(async move {
            // Counted: shutdown waits for this write before closing storage.
            let _owed = crate::agent::writes::pending().guard();
            let params = crate::eval::InsertSampleParams {
                turn_id,
                session_id,
                agent_id,
                conversation_id,
                input,
                response,
                model,
                cache_hit,
                total_tokens,
                latency_ms,
                verdict,
                risk_signals,
            };
            if let Err(e) = store.insert_sample(&params).await {
                warn!("Failed to record online turn sample: {}", e);
            }
        });
    }
}

/// Decide whether a post-turn risk scan should trigger the deep LLM Judge
/// (§八 在线质量监控).
///
/// Returns `true` only when online monitoring is enabled AND the number of
/// deterministic risk signals found on the turn is at least the configured
/// threshold. The threshold is floored at 1 so a `0` in config never silently
/// disables the judge.
pub(crate) fn should_deep_judge(risk_count: usize, enabled: bool, threshold: usize) -> bool {
    enabled && risk_count >= threshold.max(1)
}

/// Compact single-line summary of an LLM judge critique, used to surface the
/// deep-evaluation verdict in the pending badcase row and the log.
pub(crate) fn judge_summary(critique: &Critique) -> String {
    let mut parts = Vec::new();
    if !critique.dimension_scores.is_empty() {
        let mut scores = critique
            .dimension_scores
            .iter()
            .map(|(k, v)| format!("{k}={v:.2}"))
            .collect::<Vec<_>>();
        // Stable ordering so identical verdicts render identically.
        scores.sort();
        parts.push(format!("scores[{}]", scores.join(", ")));
    }
    if let Some(obs) = critique.observation.as_deref() {
        parts.push(format!("observation: {obs}"));
    }
    if parts.is_empty() {
        format!("llm judge overall_score={:.2}", critique.overall_score)
    } else {
        format!("llm judge {}", parts.join("; "))
    }
}
