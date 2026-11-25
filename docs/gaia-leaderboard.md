# GAIA test split — 提交档案

排行榜 Space：<https://huggingface.co/spaces/gaia-benchmark/leaderboard>

GAIA 的 validation leaderboard 已关闭，**只有一个数字能上榜：test split**
（301 题 = 93 L1 + 159 L2 + 49 L3）。这份文档记录提交规则、当前阻塞、以及到期那天
要做的事——规则是从 Space 的 `app.py` / `content.py` 原文读出来的，不是猜的。

## 一、当前状态

| 项 | 状态 |
|---|---|
| 答分能力 | ✅ 本地无法判分（答案私有），但**不需要**——分数由 Space 侧算 |
| 可提交账号 | ❌ `john-lightconsen`（`secrets/hf.secret`）创建于 2026-10-26 |
| 账号年龄门槛 | Space 拒绝新于 **60 天**的账号 |
| **最早可提交** | **2026-12-25（UTC）** |
| 301 题作答 | ❌ 还没跑（test split 从未跑全量） |
| 提交文件导出器 | ❌ 没有（见第五节，这是到期前唯一的真活） |

到期前不要重试提交——Space 对未满 60 天的账号直接返回
`This account is not authorized to submit on GAIA.`，而且每天只允许提交一次，
把额度浪费在必然失败的尝试上没有意义。

## 二、提交规则（Space 源码逐条摘出）

来自 `add_new_eval()`（`app.py`）与 `SUBMISSION_TEXT`（`content.py`）：

1. **只能交 test split。** `val_or_test = "test"` 是写死的，验证集入口已注释掉。
2. **必须是完整的 301 题**，且三级数量精确匹配 93 / 159 / 49：
   ```python
   ref_level_len = {"validation": {1: 53, 2: 86, 3: 26}, "test": {1: 93, 2: 159, 3: 49}}
   ```
   数量不符直接拒绝，不会部分计分。
3. **每个 `task_id` 只能出现一次。** 有重复整份退回
   （`There are duplicates in your submission.`）。→ 提交跑批**必须 `--trials 1`**。
4. **文件格式是 JSONL**，每行：
   ```json
   {"task_id": "…", "model_answer": "…", "reasoning_trace": "…"}
   ```
   `task_id` 与 `model_answer` 必填，`reasoning_trace` 可选。
   `task_id` 用**官方 task_id**（uuid 全串），不是本仓库的 `gaia_test_l123_xxxxxxxx`。
5. **每天一次**：`You already submitted once today, please try again tomorrow.`
6. **org/model 组合一次性**：`(organisation, model)` 若已判过分，不能再交第二次
   （`This model has been already submitted.`）。→ **第一次就要跑对**，名字想好。
7. **需要有效邮箱**，且邮箱会私下保存、出问题用来联系。
8. 判分用官方的 `question_scorer`，与我们 vendored 的同一份
   （`evals/importers/gaia_scorer.py`）。提交后需等几小时刷新榜单。

官方还给了一段系统提示词模板，要求以 `FINAL ANSWER: [YOUR FINAL ANSWER]` 结尾——
与本仓库生成任务时用的格式一致（`evals/importers/gaia.py` 的 `ANSWER_INSTRUCTION`）。

## 三、为什么本地不需要判分

`Final answer` 列在 test split 里全是 `?`，所以本地 pass rate 永远是 0%，
这一点见 `docs/benchmark.md` §M2。**提交路径不依赖本地判分**：本地只负责把
301 个作答产出来，判分发生在 Space。

所以本地跑 test split 的正确姿势是：
- 生成题目 + 下载附件（31 个附件）
- 跑 agent，收集最终回复
- 导出成 JSONL
- 上传

**不要**把 test split 的 pass rate 写进 `evals/ledger.md`。

## 四、到期那天的操作顺序

```bash
# 0. 先确认账号年龄门槛已过（应 ≥60 天）
curl -s -H "Authorization: Bearer $(sed -n '2p' secrets/hf.secret)" \
  https://huggingface.co/api/users/john-lightconsen/overview | python3 -c \
  "import json,sys,datetime as d; c=json.load(sys.stdin)['createdAt']; \
   age=(d.datetime.now(d.timezone.utc)-d.datetime.strptime(c,'%Y-%m-%dT%H:%M:%S.%fZ').replace(tzinfo=d.timezone.utc)).days; \
   print('age', age, 'eligible', age>=60)"

# 1. 生成 301 题（含 31 个附件）
HF_TOKEN=$(sed -n '2p' secrets/hf.secret | tr -d ' \r\n') \
  ~/.cache/syscity-bench/gaia/venv/bin/python -I evals/importers/gaia.py --split test

# 2. 跑一遍，单 trial（多 trial 会触发重复检测）
syscity eval run gaia_test_l123 --full --trials 1

# 3. 导出 JSONL（导出器尚不存在，见第五节）
python3 evals/importers/gaia_submit.py --require-complete -o /tmp/gaia_test_submission.jsonl

# 4. 浏览器打开 Space → Login with Hugging Face → 填表上传
open https://huggingface.co/spaces/gaia-benchmark/leaderboard
```

第 2 步约 301 题 × 1 trial，按 L1 的实测速度估计 **10–14 小时**。跑完再导。

## 五、到期前唯一要写的东西：`evals/importers/gaia_submit.py`

现在没有这个导出器，而且直接写会遇到两个坑：

**坑 1：test split 生成器目前会拒绝。** `build_tasks()` 在
`withheld == len(tasks)` 时 `SystemExit`，这正是 test split 的特征。到 12 月要
重新打开一条「只生成、不判分」的路径。注意**不能**用「没有判分条件」的套件绕过——
`GoalCondition` 空集会让每个 trial 都算通过，跑出来的 100% 会以真成绩的样子进
ledger。正确做法是让导出器自己驱动，或者给套件加一个不进 ledger 的显式模式。

**坑 2：slug → 官方 task_id 的映射。** 产物目录是
`$TMPDIR/eval_<slug>_<trial>/response.txt`，而提交要 uuid 全串。映射现成的：
`evals/capability/gaia_test_l123_answers.json` 的键就是官方 `task_id`，值里的 `id`
是 slug，反查即可。

导出器还应做的事：
- **答案抽取**要复用 `gaia_score.py` 的 `extract_answer()`（`FINAL ANSWER:` 优先），
  否则整段散文会被当答案交上去，`question_scorer` 判错——这不是判分，只是把要提交的
  字符串取对。
- **完整性校验**：301 条、无重复、三级数量 93/159/49，缺一条就报错退出，
  不要交出残缺文件（Space 会整份退回，白扣当天额度）。
- `reasoning_trace` 可选，可以放 `response.txt` 里 `FINAL ANSWER:` 之前的部分。

## 六、判分口径提醒

Space 侧用的是同一个 `question_scorer`，所以**提交上去的分数 = 本地应有的分数**
（前提是作答真的从 `FINAL ANSWER:` 抽出来）。这不同于 validation 那种
"scaffolding 敏感、不能和榜单比"的讨论——test split 的数字**就是**榜单数字，
可以公开发。

但注意两点，写发布文案时别说过头：
- 榜单上模型名与 org 会公开，且**不可重交**。
- 分数是 syscity 这套 scaffolding（系统提示、工具集、30 轮上限）跑出来的，
  leaderboard 的表里有 model family / system prompt / URL 三列，应当如实填。
