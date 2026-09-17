//! RCA Pipeline — root cause analysis for agent failures.
//!
//! Implements the 5-step RCA workflow from §07:
//! 1. Evidence collection (build context from trace)
//! 2. Scope narrowing (ProblemPhenomenon × CandidateModule mapping)
//! 3. Module diagnosis (rule-based + LLM-assisted)
//! 4. Responsibility determination (three-layer strategy)
//! 5. Structured persistence (to MemoryStore)
//!
//! Badcase dual-entry: auto-detected by EvalHarness + manual submission.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::agent::reflection::critic::Critic;
use crate::agent::turns::ToolCallRecord;
use crate::agent::Agent;
use crate::eval::harness::TrialResult;
use crate::Result;

// ── Problem Phenomenon ─────────────────────────────────────────────────

/// Phenomenon-layer classification (§07 mapping table).
///
/// What the user sees as the failure. Used as the entry point
/// for scope narrowing during RCA.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ProblemPhenomenon {
    /// Off-topic: user question is clear but agent replies off-topic.
    #[serde(rename = "non_responsive")]
    NonResponsive,
    /// Entity not clarified: order ID, product, etc. not disambiguated.
    #[serde(rename = "order_not_clarified")]
    OrderNotClarified,
    /// Factual error: correct info exists in KB but response is wrong.
    #[serde(rename = "factual_error")]
    FactualError,
    /// Over-promise: committing beyond authority or business rules.
    #[serde(rename = "over_promise")]
    OverPromise,
    /// Critical tool not called.
    #[serde(rename = "tool_not_called")]
    ToolNotCalled,
    /// Tool call order violated (e.g. SOP requires A→B, did B→A).
    #[serde(rename = "tool_wrong_order")]
    ToolWrongOrder,
    /// Hallucination: response content not supported by trace evidence.
    #[serde(rename = "hallucination")]
    Hallucination,
    /// Improper refusal: refused when it shouldn't, or wrong reason.
    #[serde(rename = "refusal_error")]
    RefusalError,
}

// ── Candidate Module ───────────────────────────────────────────────────

/// A syscity functional module that could be the root cause.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CandidateModule {
    IntentRecognition,
    SlotFilling,
    ContextMemory,
    Retrieval,
    ToolSelection,
    ToolExecution,
    ParameterConstruction,
    Reasoning,
    ResponseGeneration,
    PolicyEnforcement,
    SystemInfra,
}

impl CandidateModule {
    /// Return all modules (for fallback when mapping has no hit).
    pub fn all() -> Vec<Self> {
        vec![
            Self::IntentRecognition,
            Self::SlotFilling,
            Self::ContextMemory,
            Self::Retrieval,
            Self::ToolSelection,
            Self::ToolExecution,
            Self::ParameterConstruction,
            Self::Reasoning,
            Self::ResponseGeneration,
            Self::PolicyEnforcement,
            Self::SystemInfra,
        ]
    }
}

impl std::fmt::Display for CandidateModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::IntentRecognition => write!(f, "IntentRecognition"),
            Self::SlotFilling => write!(f, "SlotFilling"),
            Self::ContextMemory => write!(f, "ContextMemory"),
            Self::Retrieval => write!(f, "Retrieval"),
            Self::ToolSelection => write!(f, "ToolSelection"),
            Self::ToolExecution => write!(f, "ToolExecution"),
            Self::ParameterConstruction => write!(f, "ParameterConstruction"),
            Self::Reasoning => write!(f, "Reasoning"),
            Self::ResponseGeneration => write!(f, "ResponseGeneration"),
            Self::PolicyEnforcement => write!(f, "PolicyEnforcement"),
            Self::SystemInfra => write!(f, "SystemInfra"),
        }
    }
}

// ── Module Verdict ─────────────────────────────────────────────────────

/// Result of diagnosing a single candidate module.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModuleVerdict {
    /// Module functioned correctly.
    Pass,
    /// Module mostly passed but has minor issues.
    SoftPass(String),
    /// Module failed — root cause located.
    Fail(String),
    /// Unable to determine (needs deeper analysis).
    Unknown,
}

// ── Badcase Entry ──────────────────────────────────────────────────────

/// How this badcase entered the RCA pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum BadcaseEntry {
    /// Auto-detected by EvalHarness (failed trial).
    AutoDetected,
    /// Auto-collected online: deterministic risk signal fired on a turn.
    OnlineRisk,
    /// Auto-collected online: a user disliked (👎) this turn.
    HumanVote,
    /// Manually submitted by operator/QA/engineer.
    ManualSubmit {
        reporter: String,
        description: String,
    },
}

// ── RCA Input ──────────────────────────────────────────────────────────

/// Input to the RCA pipeline.
#[derive(Debug, Clone)]
pub struct RcaInput {
    pub run_id: String,
    pub task_id: String,
    pub user_input: String,
    pub response: String,
    pub tool_calls: Vec<ToolCallRecord>,
    pub scoring: Option<TrialResult>,
    pub entry: BadcaseEntry,
}

// ── RCA Result ─────────────────────────────────────────────────────────

/// Complete RCA output with three-layer attribution and structured fix.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RcaResult {
    // ── Three-layer attribution ──
    /// Phenomenon: what the user sees.
    pub phenomenon: String,
    /// Process: which step first deviated in the trace.
    pub process_deviation: String,
    /// Responsibility: which module/owner should own this.
    pub responsibility: String,

    // ── Structured attribution ──
    pub problem_category: String,
    pub problem_enumeration: String,
    pub responsibility_module: CandidateModule,
    pub sub_responsibility: Option<CandidateModule>,

    // ── Evidence & fix ──
    pub evidence_chain: Vec<String>,
    pub fix_suggestion: String,
    pub confidence: f64,

    // ── Meta ──
    pub analysis_duration_ms: u64,
    pub entry: BadcaseEntry,
    pub completed_at: SystemTime,
}

// ── Knowledge Base ─────────────────────────────────────────────────────

/// A historical resolution record for RCA knowledge base.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RcaKnowledgeBaseEntry {
    pub problem_enumeration: String,
    pub responsibility_module: CandidateModule,
    pub known_fixes: Vec<String>,
    pub hit_count: u32,
    pub last_resolution: Option<String>,
}

/// Knowledge base of historical RCA results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RcaKnowledgeBase {
    entries: Vec<RcaKnowledgeBaseEntry>,
}

impl Default for RcaKnowledgeBase {
    fn default() -> Self {
        let mut kb = Self { entries: Vec::new() };
        kb.seed_defaults();
        kb
    }
}

impl RcaKnowledgeBase {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the KB with common failure patterns and known fixes.
    fn seed_defaults(&mut self) {
        let defaults = vec![
            RcaKnowledgeBaseEntry {
                problem_enumeration: "关键工具未调用".into(),
                responsibility_module: CandidateModule::ToolSelection,
                known_fixes: vec![
                    "检查工具选择 prompt 中是否包含所有必需工具".into(),
                    "确认工具注册列表包含目标工具".into(),
                    "添加工具选择约束词 (必须/应当调用)".into(),
                ],
                hit_count: 0,
                last_resolution: None,
            },
            RcaKnowledgeBaseEntry {
                problem_enumeration: "意图识别错误".into(),
                responsibility_module: CandidateModule::IntentRecognition,
                known_fixes: vec![
                    "增强意图识别 prompt 中的边界案例描述".into(),
                    "添加否定/排除规则以避免误匹配".into(),
                ],
                hit_count: 0,
                last_resolution: None,
            },
            RcaKnowledgeBaseEntry {
                problem_enumeration: "知识召回不足".into(),
                responsibility_module: CandidateModule::Retrieval,
                known_fixes: vec![
                    "增加检索工具的结果数量限制".into(),
                    "优化检索 query 构造方式".into(),
                    "添加 fallback 检索策略".into(),
                ],
                hit_count: 0,
                last_resolution: None,
            },
            RcaKnowledgeBaseEntry {
                problem_enumeration: "回复质量缺陷".into(),
                responsibility_module: CandidateModule::ResponseGeneration,
                known_fixes: vec![
                    "优化回复模板约束条件".into(),
                    "添加事实一致性检查 prompt".into(),
                    "限制回复长度和格式要求".into(),
                ],
                hit_count: 0,
                last_resolution: None,
            },
            RcaKnowledgeBaseEntry {
                problem_enumeration: "策略违规".into(),
                responsibility_module: CandidateModule::PolicyEnforcement,
                known_fixes: vec![
                    "更新策略规则定义".into(),
                    "添加 guardrail 检查逻辑".into(),
                    "实现拒绝响应模板".into(),
                ],
                hit_count: 0,
                last_resolution: None,
            },
            RcaKnowledgeBaseEntry {
                problem_enumeration: "过度承诺".into(),
                responsibility_module: CandidateModule::PolicyEnforcement,
                known_fixes: vec![
                    "在系统 prompt 中添加权限边界描述".into(),
                    "禁止使用确定性/保证性词汇".into(),
                    "添加知识库权限校验机制".into(),
                ],
                hit_count: 0,
                last_resolution: None,
            },
            RcaKnowledgeBaseEntry {
                problem_enumeration: "幻觉回复".into(),
                responsibility_module: CandidateModule::ResponseGeneration,
                known_fixes: vec![
                    "强制要求回复仅基于工具返回结果".into(),
                    "添加来源引用要求".into(),
                    "实施事实验证步骤".into(),
                ],
                hit_count: 0,
                last_resolution: None,
            },
            RcaKnowledgeBaseEntry {
                problem_enumeration: "拒绝策略不当".into(),
                responsibility_module: CandidateModule::PolicyEnforcement,
                known_fixes: vec![
                    "细化拒绝策略分类 (可回答/需澄清/坚决拒绝)".into(),
                    "添加可回答场景的豁免规则".into(),
                ],
                hit_count: 0,
                last_resolution: None,
            },
        ];
        for entry in defaults {
            self.entries.push(entry);
        }
    }

    /// Look up known fixes for a given problem enumeration + module.
    pub fn lookup(
        &self,
        enumeration: &str,
        module: &CandidateModule,
    ) -> Option<&RcaKnowledgeBaseEntry> {
        self.entries
            .iter()
            .find(|e| e.problem_enumeration == enumeration && e.responsibility_module == *module)
    }

    /// Add or update an entry.
    pub fn record(&mut self, entry: RcaKnowledgeBaseEntry) {
        // Update hit count if exists
        if let Some(existing) = self.entries.iter_mut().find(|e| {
            e.problem_enumeration == entry.problem_enumeration
                && e.responsibility_module == entry.responsibility_module
        }) {
            existing.hit_count += 1;
            existing.known_fixes = entry.known_fixes;
            existing.last_resolution = entry.last_resolution;
        } else {
            self.entries.push(entry);
        }
    }
}

// ── RCA Pipeline ───────────────────────────────────────────────────────

/// The 5-step RCA pipeline.
pub struct RcaPipeline {
    pub agent: Arc<Agent>,
    pub critic: Option<Critic>,
    pub module_mapping: HashMap<ProblemPhenomenon, Vec<CandidateModule>>,
    pub knowledge_base: RcaKnowledgeBase,
}

impl RcaPipeline {
    /// Build the mapping table (ProblemPhenomenon × CandidateModule).
    pub fn build_module_mapping() -> HashMap<ProblemPhenomenon, Vec<CandidateModule>> {
        let mut m = HashMap::new();
        m.insert(
            ProblemPhenomenon::NonResponsive,
            vec![
                CandidateModule::IntentRecognition,
                CandidateModule::ContextMemory,
            ],
        );
        m.insert(
            ProblemPhenomenon::OrderNotClarified,
            vec![CandidateModule::SlotFilling, CandidateModule::ContextMemory],
        );
        m.insert(
            ProblemPhenomenon::FactualError,
            vec![
                CandidateModule::Retrieval,
                CandidateModule::Reasoning,
                CandidateModule::ResponseGeneration,
            ],
        );
        m.insert(
            ProblemPhenomenon::OverPromise,
            vec![
                CandidateModule::PolicyEnforcement,
                CandidateModule::ResponseGeneration,
            ],
        );
        m.insert(
            ProblemPhenomenon::ToolNotCalled,
            vec![
                CandidateModule::ToolSelection,
                CandidateModule::IntentRecognition,
            ],
        );
        m.insert(
            ProblemPhenomenon::ToolWrongOrder,
            vec![CandidateModule::ToolSelection, CandidateModule::Reasoning],
        );
        m.insert(
            ProblemPhenomenon::Hallucination,
            vec![
                CandidateModule::Retrieval,
                CandidateModule::ResponseGeneration,
            ],
        );
        m.insert(
            ProblemPhenomenon::RefusalError,
            vec![
                CandidateModule::PolicyEnforcement,
                CandidateModule::IntentRecognition,
            ],
        );
        m
    }

    pub fn new(agent: Arc<Agent>, critic: Option<Critic>) -> Self {
        Self {
            agent,
            critic,
            module_mapping: Self::build_module_mapping(),
            knowledge_base: RcaKnowledgeBase::new(),
        }
    }

    /// Run the full 5-step RCA analysis.
    pub async fn analyze(&self, input: RcaInput) -> Result<RcaResult> {
        let start = std::time::Instant::now();

        // ── Step 1: Evidence collection ────────────────────────────
        let evidence = self.collect_evidence(&input).await;

        // ── Step 2: Scope narrowing ────────────────────────────────
        let phenomenon = self.detect_phenomenon(&input).await;
        let candidates = self
            .module_mapping
            .get(&phenomenon)
            .cloned()
            .unwrap_or_else(CandidateModule::all);

        info!("RCA: phenomenon={:?}, candidates={:?}", phenomenon, candidates);

        // ── Step 3: Module diagnosis ───────────────────────────────
        let mut diagnoses = Vec::new();
        for module in &candidates {
            let verdict = self.diagnose_module(module, &input, &evidence).await;
            diagnoses.push((module.clone(), verdict));
        }

        // ── Step 4: Responsibility determination ───────────────────
        // Layer 1: Direct FAIL attribution
        let primary_fail = diagnoses
            .iter()
            .find(|(_, v)| matches!(v, ModuleVerdict::Fail(_)));
        // Layer 2: Rule matching (if no direct fail)
        let rule_match = if primary_fail.is_none() {
            self.match_rules(&input).await
        } else {
            None
        };

        let (main_module, sub_module, category, enumeration, fix) =
            if let Some((mod_, ModuleVerdict::Fail(reason))) = primary_fail {
                let (cat, enum_, fix) = self.lookup_fix(mod_, reason);
                (mod_.clone(), None, cat, enum_, fix)
            } else if let Some(rm) = rule_match {
                rm
            } else {
                // Layer 3: LLM summarization
                let first_fail = diagnoses
                    .first()
                    .map(|(m, _)| m.clone())
                    .unwrap_or(CandidateModule::ResponseGeneration);
                (
                    first_fail,
                    None,
                    "语义理解".into(),
                    "无法明确归因".into(),
                    "请人工分析此案例".into(),
                )
            };

        let elapsed = start.elapsed();
        let confidence = self.compute_confidence(&diagnoses);

        // ── Step 5: Return structured result ───────────────────────
        Ok(RcaResult {
            phenomenon: format!("{:?}", phenomenon),
            process_deviation: self.describe_process_deviation(&input),
            responsibility: self.determine_responsibility(&diagnoses),
            problem_category: category,
            problem_enumeration: enumeration,
            responsibility_module: main_module,
            sub_responsibility: sub_module,
            evidence_chain: evidence,
            fix_suggestion: fix,
            confidence,
            analysis_duration_ms: elapsed.as_millis() as u64,
            entry: input.entry,
            completed_at: SystemTime::now(),
        })
    }

    // ── Step 1: Evidence collection ────────────────────────────────

    async fn collect_evidence(&self, input: &RcaInput) -> Vec<String> {
        let mut ev = vec![
            format!("User input: {}", input.user_input),
            format!("Agent response: {}", input.response),
        ];

        // Tool call evidence from ToolCallRecord
        for tc in &input.tool_calls {
            ev.push(format!(
                "Tool call: {}({}) → success={} in {}ms",
                tc.name, tc.args, tc.success, tc.duration_ms
            ));
        }

        // Try to get detailed turn data from thread_map
        let conv_id = format!("eval_{}_{}", input.task_id, input.run_id);
        let map = self.agent.thread_map.lock().await;
        if let Some(thread) = map.get(&conv_id) {
            for turn in &thread.turns {
                if !turn.assistant_response.is_empty() {
                    ev.push(format!("Turn {} response: {}", turn.index, turn.assistant_response));
                }
            }
        }
        drop(map);

        ev
    }

    // ── Step 2: Phenomenon detection ───────────────────────────────

    async fn detect_phenomenon(&self, input: &RcaInput) -> ProblemPhenomenon {
        // Rule priority: deterministic matches

        // 1. Tool not called
        let tool_names: Vec<&str> = input.tool_calls.iter().map(|t| t.name.as_str()).collect();
        if tool_names.is_empty() && !input.response.is_empty() {
            return ProblemPhenomenon::ToolNotCalled;
        }

        // 2. Match from scoring critique weaknesses
        if let Some(ref scoring) = input.scoring {
            if let Some(ref critique) = scoring.critique {
                for w in &critique.weaknesses {
                    let wl = w.to_lowercase();
                    if wl.contains("承诺") || wl.contains("overpromise") {
                        return ProblemPhenomenon::OverPromise;
                    }
                    if wl.contains("幻觉") || wl.contains("hallucination") || wl.contains("编造")
                    {
                        return ProblemPhenomenon::Hallucination;
                    }
                    if wl.contains("事实") || wl.contains("factual") || wl.contains("错误") {
                        return ProblemPhenomenon::FactualError;
                    }
                    if wl.contains("拒绝") || wl.contains("refus") {
                        return ProblemPhenomenon::RefusalError;
                    }
                    if wl.contains("偏题") || wl.contains("off-topic") {
                        return ProblemPhenomenon::NonResponsive;
                    }
                }
            }
        }

        // 3. From condition check results
        if let Some(ref scoring) = input.scoring {
            for cr in &scoring.condition_results {
                if !cr.passed {
                    let al = cr.actual.to_lowercase();
                    if al.contains("no match") || al.contains("not found") {
                        return ProblemPhenomenon::FactualError;
                    }
                }
            }
        }

        ProblemPhenomenon::FactualError // fallback
    }

    // ── Step 3: Module diagnosis ───────────────────────────────────

    async fn diagnose_module(
        &self,
        module: &CandidateModule,
        input: &RcaInput,
        _evidence: &[String],
    ) -> ModuleVerdict {
        match module {
            CandidateModule::ToolSelection => {
                // Check if expected tools were called
                let called: Vec<&str> = input.tool_calls.iter().map(|t| t.name.as_str()).collect();
                if called.is_empty() && !input.response.is_empty() {
                    return ModuleVerdict::Fail("工具未被调用".into());
                }
                ModuleVerdict::Pass
            }

            CandidateModule::IntentRecognition => {
                // Simple heuristic: user input is non-empty, response is non-empty
                if input.user_input.is_empty() && !input.response.is_empty() {
                    return ModuleVerdict::Fail("空输入但产生了回复".into());
                }
                ModuleVerdict::Pass
            }

            CandidateModule::Retrieval => {
                // Check if any search/retrieval tool was called
                let has_search = input.tool_calls.iter().any(|t| {
                    t.name.contains("search")
                        || t.name.contains("fetch")
                        || t.name.contains("memory")
                });
                if !has_search && input.response.len() > 50 {
                    return ModuleVerdict::SoftPass(
                        "May need retrieval but no search tool was called".into(),
                    );
                }
                ModuleVerdict::Pass
            }

            CandidateModule::ResponseGeneration => {
                // LLM-assisted check via Critic
                if let Some(ref critic) = self.critic {
                    let prompt = format!(
                        "Evaluate whether the response faithfully reflects available evidence.\n\
                         Evidence (tool calls): {:?}\nResponse: {}\n\
                         Does the response accurately use the evidence? Answer PASS or FAIL with reason.",
                        input.tool_calls.iter().map(|t| &t.name).collect::<Vec<_>>(),
                        input.response
                    );
                    // Use a simple evaluation prompt
                    let criteria = crate::agent::reflection::types::QualityCriteria::default();
                    match critic.evaluate_trajectory(&prompt, &criteria, None).await {
                        Ok(c) if !c.passed => {
                            return ModuleVerdict::Fail(
                                "Response does not faithfully reflect tool results".into(),
                            );
                        }
                        _ => {}
                    }
                }
                ModuleVerdict::Pass
            }

            CandidateModule::PolicyEnforcement => {
                // Check for over-promise indicators
                let over_promise_keywords = ["一定", "保证", "肯定", "承诺", "绝对"];
                let response_lower = input.response.to_lowercase();
                for kw in &over_promise_keywords {
                    if response_lower.contains(kw) {
                        return ModuleVerdict::Fail(format!("包含过度承诺关键词 '{}'", kw));
                    }
                }
                ModuleVerdict::Pass
            }

            CandidateModule::SlotFilling => ModuleVerdict::Pass,
            CandidateModule::ContextMemory => ModuleVerdict::Pass,
            CandidateModule::ToolExecution => ModuleVerdict::Pass,
            CandidateModule::ParameterConstruction => ModuleVerdict::Pass,
            CandidateModule::Reasoning => ModuleVerdict::Pass,
            CandidateModule::SystemInfra => ModuleVerdict::Pass,
        }
    }

    // ── Layer 2: Rule matching ─────────────────────────────────────

    async fn match_rules(
        &self,
        input: &RcaInput,
    ) -> Option<(CandidateModule, Option<CandidateModule>, String, String, String)> {
        let tool_names: Vec<&str> = input.tool_calls.iter().map(|t| t.name.as_str()).collect();

        // If no tools were called and response is short → likely Policy issue
        if tool_names.is_empty() && input.response.len() < 20 {
            return Some((
                CandidateModule::PolicyEnforcement,
                None,
                "风险防控".into(),
                "不当拒答".into(),
                "检查拒答策略和 Guardrail 规则".into(),
            ));
        }

        None
    }

    // ── Fix lookup ─────────────────────────────────────────────────

    fn lookup_fix(&self, module: &CandidateModule, reason: &str) -> (String, String, String) {
        // Try knowledge base first
        let enumeration = match module {
            CandidateModule::ToolSelection => "关键工具未调用",
            CandidateModule::IntentRecognition => "意图识别错误",
            CandidateModule::Retrieval => "知识召回不足",
            CandidateModule::ResponseGeneration => "回复质量缺陷",
            CandidateModule::PolicyEnforcement => "策略违规",
            _ => "未分类错误",
        };

        if let Some(entry) = self.knowledge_base.lookup(enumeration, module) {
            let fix = if entry.known_fixes.is_empty() {
                format!("模块 {:?} 诊断失败: {}. 请检查配置、Prompt 或实现。", module, reason)
            } else {
                format!("建议修复: {}。诊断详情: {}", entry.known_fixes.join("; "), reason)
            };
            let category = module_category(module);
            return (category.into(), enumeration.into(), fix);
        }

        let category = module_category(module);
        let enumeration = match module {
            CandidateModule::ToolSelection => "关键工具未调用",
            CandidateModule::IntentRecognition => "意图识别错误",
            CandidateModule::Retrieval => "知识召回不足",
            CandidateModule::ResponseGeneration => "回复质量缺陷",
            CandidateModule::PolicyEnforcement => "策略违规",
            _ => "未分类错误",
        };

        let fix =
            format!("模块 {:?} 诊断失败: {}。请检查模块配置、Prompt 或实现。", module, reason);

        (category.into(), enumeration.into(), fix)
    }

    /// Analyze process deviation — trace through tool calls to find where
    /// execution first diverged from expected flow.
    fn describe_process_deviation(&self, input: &RcaInput) -> String {
        let mut details = Vec::new();

        // Check for missing tool calls
        if input.tool_calls.is_empty() && !input.response.is_empty() {
            return "过程偏离: 未调用任何工具就直接生成回复".into();
        }

        // Analyze each tool call for failures
        for tc in &input.tool_calls {
            if !tc.success {
                details.push(format!(
                    "工具调用失败: {}(参数: {}) - 耗时 {}ms",
                    tc.name, tc.args, tc.duration_ms
                ));
            }
        }

        // Check response-to-tool ratio
        if !input.tool_calls.is_empty() && input.response.len() < 10 {
            details.push("过程偏离: 调用了工具但回复过短".into());
        }

        if details.is_empty() {
            "未检测到明显过程偏离".into()
        } else {
            details.join("; ")
        }
    }

    /// Determine responsibility layer — which functional module should
    /// own this failure, based on evidence strength.
    fn determine_responsibility(&self, diagnoses: &[(CandidateModule, ModuleVerdict)]) -> String {
        let fails: Vec<&CandidateModule> = diagnoses
            .iter()
            .filter_map(|(m, v)| matches!(v, ModuleVerdict::Fail(_)).then_some(m))
            .collect();

        if fails.is_empty() {
            "无法明确归因 — 需要人工分析".into()
        } else if fails.len() == 1 {
            format!("{:?}", fails[0])
        } else {
            format!("{:?} (主导), {:?} (协同)", fails[0], fails[1])
        }
    }

    /// Compute confidence based on evidence strength.
    fn compute_confidence(&self, diagnoses: &[(CandidateModule, ModuleVerdict)]) -> f64 {
        let has_fail = diagnoses
            .iter()
            .any(|(_, v)| matches!(v, ModuleVerdict::Fail(_)));
        let has_soft = diagnoses
            .iter()
            .any(|(_, v)| matches!(v, ModuleVerdict::SoftPass(_)));
        let fail_count = diagnoses
            .iter()
            .filter(|(_, v)| matches!(v, ModuleVerdict::Fail(_)))
            .count();

        if has_fail && fail_count == 1 {
            0.85 // Single clear root cause
        } else if has_fail {
            0.70 // Multiple failures — less certain
        } else if has_soft {
            0.50 // Only soft signals
        } else {
            0.30 // No clear signals
        }
    }

    /// Persist RCA result to MemoryStore.
    pub async fn persist(&self, result: RcaResult) -> Result<()> {
        if self.agent.memory_store().is_some() {
            // Persist to memory store for later querying
            let content = serde_json::to_string(&result)?;
            info!(
                "RCA result persisted: {} ({})",
                result.problem_enumeration, result.responsibility_module
            );
            // In a full implementation, this would write to MemoryStore.
            // For now, log the result.
            tracing::debug!("RCA content: {}", content);
        }
        Ok(())
    }
}

/// Map a CandidateModule to its Chinese category label.
fn module_category(module: &CandidateModule) -> &'static str {
    match module {
        CandidateModule::ToolSelection => "工具执行",
        CandidateModule::IntentRecognition => "语义理解",
        CandidateModule::Retrieval => "知识召回",
        CandidateModule::ResponseGeneration => "回复生成",
        CandidateModule::PolicyEnforcement => "风险防控",
        CandidateModule::SlotFilling => "槽位抽取",
        CandidateModule::ContextMemory => "上下文记忆",
        CandidateModule::ToolExecution => "工具执行",
        CandidateModule::ParameterConstruction => "参数构造",
        CandidateModule::Reasoning => "推理决策",
        CandidateModule::SystemInfra => "系统基础设施",
    }
}

/// Map a CandidateModule to its responsible owner/team string.
pub fn module_to_owner(module: &CandidateModule) -> &'static str {
    match module {
        CandidateModule::IntentRecognition => "nlp_team",
        CandidateModule::SlotFilling => "nlp_team",
        CandidateModule::ContextMemory => "memory_team",
        CandidateModule::Retrieval => "knowledge_team",
        CandidateModule::ToolSelection => "tool_team",
        CandidateModule::ToolExecution => "tool_team",
        CandidateModule::ParameterConstruction => "tool_team",
        CandidateModule::Reasoning => "reasoning_team",
        CandidateModule::ResponseGeneration => "generation_team",
        CandidateModule::PolicyEnforcement => "policy_team",
        CandidateModule::SystemInfra => "infra_team",
    }
}

/// Helper: generate RcaInput from EvalHarness TrialResult.
pub fn rca_input_from_trial(task_id: &str, trial: &TrialResult, task_input: &str) -> RcaInput {
    let tool_records: Vec<ToolCallRecord> = trial
        .tool_calls
        .iter()
        .map(|tc| ToolCallRecord {
            name: tc.name.clone(),
            args: tc.args.clone(),
            result: tc.result.clone(),
            success: tc.success,
            duration_ms: tc.duration_ms,
        })
        .collect();

    RcaInput {
        run_id: task_id.to_string(),
        task_id: task_id.to_string(),
        user_input: task_input.to_string(),
        response: trial.response.clone(),
        tool_calls: tool_records,
        scoring: Some(trial.clone()),
        entry: BadcaseEntry::AutoDetected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_module_mapping_coverage() {
        let map = RcaPipeline::build_module_mapping();
        // All phenomena should map to at least one module
        let phenomena = vec![
            ProblemPhenomenon::NonResponsive,
            ProblemPhenomenon::OrderNotClarified,
            ProblemPhenomenon::FactualError,
            ProblemPhenomenon::OverPromise,
            ProblemPhenomenon::ToolNotCalled,
            ProblemPhenomenon::ToolWrongOrder,
            ProblemPhenomenon::Hallucination,
            ProblemPhenomenon::RefusalError,
        ];
        for p in phenomena {
            assert!(map.contains_key(&p), "Missing mapping for {:?}", p);
            assert!(!map[&p].is_empty(), "Empty candidate list for {:?}", p);
        }
    }

    #[test]
    fn test_knowledge_base_seeded() {
        let kb = RcaKnowledgeBase::new();
        // KB should have default entries
        assert!(kb
            .lookup("关键工具未调用", &CandidateModule::ToolSelection)
            .is_some());
        assert!(kb
            .lookup("意图识别错误", &CandidateModule::IntentRecognition)
            .is_some());
        assert!(kb
            .lookup("知识召回不足", &CandidateModule::Retrieval)
            .is_some());
        assert!(kb
            .lookup("回复质量缺陷", &CandidateModule::ResponseGeneration)
            .is_some());
        assert!(kb
            .lookup("策略违规", &CandidateModule::PolicyEnforcement)
            .is_some());
        // Unknown pair should return None
        assert!(kb
            .lookup("不存在的问题", &CandidateModule::SystemInfra)
            .is_none());
    }

    #[test]
    fn test_knowledge_base_record_updates_hit_count() {
        let mut kb = RcaKnowledgeBase::new();
        let existing = kb.lookup("关键工具未调用", &CandidateModule::ToolSelection);
        let initial_hits = existing.map(|e| e.hit_count).unwrap_or(0);

        kb.record(RcaKnowledgeBaseEntry {
            problem_enumeration: "关键工具未调用".into(),
            responsibility_module: CandidateModule::ToolSelection,
            known_fixes: vec!["测试修复".into()],
            hit_count: 0,
            last_resolution: None,
        });

        let updated = kb.lookup("关键工具未调用", &CandidateModule::ToolSelection);
        assert_eq!(updated.unwrap().hit_count, initial_hits + 1);
    }

    #[test]
    fn test_knowledge_base() {
        let mut kb = RcaKnowledgeBase::new();
        kb.record(RcaKnowledgeBaseEntry {
            problem_enumeration: "关键工具未调用".into(),
            responsibility_module: CandidateModule::ToolSelection,
            known_fixes: vec!["修改工具选择 Prompt".into()],
            hit_count: 1,
            last_resolution: None,
        });

        let result = kb.lookup("关键工具未调用", &CandidateModule::ToolSelection);
        assert!(result.is_some());
        assert_eq!(result.unwrap().known_fixes[0], "修改工具选择 Prompt");
    }

    #[test]
    fn test_module_to_owner_all_modules() {
        for m in CandidateModule::all() {
            let owner = module_to_owner(&m);
            assert!(!owner.is_empty(), "No owner for {:?}", m);
        }
    }

    #[test]
    fn test_module_to_owner_consistency() {
        // module_to_owner should return stable, consistent results
        assert_eq!(module_to_owner(&CandidateModule::ToolSelection), "tool_team");
        assert_eq!(module_to_owner(&CandidateModule::ToolExecution), "tool_team");
        assert_eq!(module_to_owner(&CandidateModule::ParameterConstruction), "tool_team");
        assert_eq!(module_to_owner(&CandidateModule::IntentRecognition), "nlp_team");
        assert_eq!(module_to_owner(&CandidateModule::SlotFilling), "nlp_team");
        assert_eq!(module_to_owner(&CandidateModule::ContextMemory), "memory_team");
        assert_eq!(module_to_owner(&CandidateModule::Retrieval), "knowledge_team");
        assert_eq!(module_to_owner(&CandidateModule::Reasoning), "reasoning_team");
        assert_eq!(module_to_owner(&CandidateModule::ResponseGeneration), "generation_team");
        assert_eq!(module_to_owner(&CandidateModule::PolicyEnforcement), "policy_team");
        assert_eq!(module_to_owner(&CandidateModule::SystemInfra), "infra_team");
    }
}
