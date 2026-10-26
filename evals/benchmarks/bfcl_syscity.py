"""Register a Syscity gateway as a BFCL model, then dispatch to the `bfcl` CLI.

`bfcl_eval` only knows about models listed in its own `constants/model_config.py`,
so a self-hosted OpenAI-compatible endpoint has to be injected into
`MODEL_CONFIG_MAPPING` before the CLI imports it. That is all this file does;
everything after the injection is the unmodified upstream CLI, so the numbers it
prints are the official evaluator's.

Setup (once, outside the repo):

    uv venv ~/.cache/syscity-bench/bfcl
    uv pip install --python ~/.cache/syscity-bench/bfcl/bin/python bfcl-eval
    # PyPI packaging pulls qwen_agent in and does not declare its optional dep:
    uv pip install --python ~/.cache/syscity-bench/bfcl/bin/python soundfile

Run (gateway on :18081 built with `mode: raw`):

    export BFCL_PROJECT_ROOT=~/.cache/syscity-bench/run   # keep results out of the repo
    export OPENAI_BASE_URL=http://127.0.0.1:18081/v1
    export OPENAI_API_KEY=local                           # gateway auth disabled
    export BFCL_SYSCITY_MODEL=syscity-agent               # sentinel → gateway default model

    python evals/benchmarks/bfcl_syscity.py generate --model syscity \\
        --test-category simple_python,irrelevance,multi_turn_base
    python evals/benchmarks/bfcl_syscity.py evaluate --model syscity \\
        --test-category simple_python,irrelevance,multi_turn_base

Two things make the difference between "runs" and "scores":

* **`mode` default.** BFCL sends `tools` and no `mode` field. The gateway defaults
  such requests to `raw` (see `OpenAiChatRequest::effective_mode`); if it did not,
  the tools would be dropped and every call turn would come back empty.
* **`underscore_to_dot`.** The OpenAI wire forbids `.` in function names, so BFCL
  sends the dataset's `math.factorial` as `math_factorial`. Without this flag the
  checker looks for a function named `math.factorial` in output that calls
  `math_factorial`, and marks correct calls wrong.

A subset run (fast smoke) works through BFCL's id file: write
`$BFCL_PROJECT_ROOT/test_case_ids_to_generate.json` as
`{"simple_python": ["simple_python_0", ...]}`, pass `--run-ids` to `generate`,
and `--partial-eval` to `evaluate`.
"""

import os

from bfcl_eval.constants.model_config import MODEL_CONFIG_MAPPING, ModelConfig
from bfcl_eval.model_handler.api_inference.openai_completion import (
    OpenAICompletionsHandler,
)

MODEL_ID = "syscity"

MODEL_CONFIG_MAPPING[MODEL_ID] = ModelConfig(
    model_name=os.getenv("BFCL_SYSCITY_MODEL", "syscity-agent"),
    display_name="Syscity",
    url="https://github.com/lightconsen/syscity",
    org="lightconsen",
    license="MIT",
    model_handler=OpenAICompletionsHandler,
    is_fc_model=True,
    underscore_to_dot=True,
)

# Imported after the registration, because the CLI snapshots the mapping at
# import time.
from bfcl_eval.__main__ import cli  # noqa: E402

if __name__ == "__main__":
    cli()
