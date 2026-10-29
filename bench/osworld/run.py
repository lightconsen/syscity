"""Run OSWorld with syscity as the agent.

The loop is OSWorld's own: `lib_run_single.run_single_example` resets the VM,
calls the agent's `predict`, executes what it returns, and finally runs the
task's official evaluator. The only new thing here is which agent it is driving
(`SyscityAgent`, see sycity_agent.py) and the report at the end, which keeps
infrastructure errors out of the success rate's denominator.

Usage, from inside an OSWorld checkout (it must be importable — this script
only adds its root to `sys.path`, it does not vendor it):

    cd /path/to/OSWorld
    python /path/to/syscity/bench/osworld/run.py \
        --osworld-root . \
        --provider-name docker --path-to-vm /path/to/Ubuntu.vmx \
        --split test_small --max-steps 60 \
        --syscity /path/to/syscity/target/release/syscity \
        --provider deepseek --model deepseek-v4-flash \
        --result-dir ./results/syscity

Exit code is 0 when the run completed, whatever the scores were. A run that
could not start at all (bad path, missing upstream) exits 1.
"""

from __future__ import annotations

import argparse
import json
import logging
import os
import sys
import traceback

SPLITS = {
    "test_small": "test_small.json",
    "test_all": "test_all.json",
    "test_nogdrive": "test_nogdrive.json",
    "test_infeasible": "test_infeasible.json",
}

logger = logging.getLogger("desktopenv.syscity")


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description="OSWorld × syscity (replay)")
    parser.add_argument(
        "--osworld-root",
        default=".",
        help="OSWorld checkout to import lib_run_single/desktop_env from",
    )
    parser.add_argument(
        "--test-config-base-dir", default="evaluation_examples", help="examples live here"
    )
    parser.add_argument("--split", choices=sorted(SPLITS), default="test_small")
    parser.add_argument("--meta", default=None, help="explicit meta json path (overrides --split)")
    parser.add_argument("--domain", default="all")
    parser.add_argument("--ids", default=None, help="comma-separated example ids to keep")

    # Environment
    parser.add_argument("--provider-name", default="vmware")
    parser.add_argument("--path-to-vm", default=None)
    parser.add_argument("--headless", action="store_true")
    parser.add_argument("--screen-width", type=int, default=1920)
    parser.add_argument("--screen-height", type=int, default=1080)
    parser.add_argument(
        "--sleep-after-execution",
        type=float,
        default=0.5,
        help="seconds the harness waits after each replayed action",
    )
    parser.add_argument(
        "--vm-secret-mount", action="append", default=None, help="local_path:guest_path"
    )

    # Agent
    parser.add_argument("--syscity", default="syscity", help="path to the syscity binary")
    parser.add_argument("--max-steps", type=int, default=60, help="syscity's tool budget")
    parser.add_argument("--timeout", type=float, default=1800.0, help="seconds per task")
    parser.add_argument("--provider", default=None, help="syscity provider name")
    parser.add_argument("--model", default=None)
    parser.add_argument("--base-url", default=None)

    # Output
    parser.add_argument("--result-dir", default="./results/syscity")
    parser.add_argument(
        "--resume", action="store_true", help="skip tasks that already have a result.txt"
    )
    return parser.parse_args(argv)


def load_upstream(root):
    """Import the OSWorld modules this runner reuses.

    They are imported from the checkout, not copied: the evaluator, the step
    loop and the result-writing conventions are the benchmark's, and a local
    fork of them would make the numbers incomparable.
    """
    root = os.path.abspath(root)
    if not os.path.isfile(os.path.join(root, "lib_run_single.py")):
        raise SystemExit(
            "{0} is not an OSWorld checkout: lib_run_single.py is missing".format(root)
        )
    if root not in sys.path:
        sys.path.insert(0, root)
    try:
        import lib_run_single  # noqa: E402
        from desktop_env.desktop_env import DesktopEnv  # noqa: E402
    except ModuleNotFoundError as error:
        raise SystemExit(
            "OSWorld's own dependencies are missing ({0}); install them into the "
            "environment that runs this script: pip install -e {1}".format(error, root)
        )
    return lib_run_single, DesktopEnv


def select_examples(meta, test_config_base_dir, domain, ids):
    wanted = set(part.strip() for part in ids.split(",") if part.strip()) if ids else None
    selected = []
    for meta_domain, example_ids in meta.items():
        if domain != "all" and meta_domain != domain:
            continue
        for example_id in example_ids:
            if wanted is not None and example_id not in wanted:
                continue
            config_path = os.path.join(
                test_config_base_dir, "examples", meta_domain, example_id + ".json"
            )
            if not os.path.isfile(config_path):
                logger.warning("skipping %s/%s: %s is missing", meta_domain, example_id, config_path)
                continue
            selected.append((meta_domain, example_id, config_path))
    return selected


def main(argv=None):
    args = parse_args(argv)
    logging.basicConfig(
        level=logging.INFO, format="%(asctime)s %(levelname)s %(name)s: %(message)s"
    )

    lib_run_single, DesktopEnv = load_upstream(args.osworld_root)

    # Imported after sys.path is set, and from this file's own directory.
    sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
    from sycity_agent import SyscityAgent, SyscityRunFailed, SyscityUnavailable, summarize

    meta_path = args.meta or os.path.join(
        args.test_config_base_dir, SPLITS[args.split]
    )
    with open(meta_path, "r", encoding="utf-8") as handle:
        meta = json.load(handle)
    examples = select_examples(meta, args.test_config_base_dir, args.domain, args.ids)
    if not examples:
        raise SystemExit("no examples selected from {0}".format(meta_path))
    logger.info("%d example(s) from %s", len(examples), meta_path)

    os.makedirs(args.result_dir, exist_ok=True)

    agent = SyscityAgent(
        binary=args.syscity,
        max_steps=args.max_steps,
        provider=args.provider,
        model=args.model,
        base_url=args.base_url,
        timeout=args.timeout,
    )

    env = DesktopEnv(
        provider_name=args.provider_name,
        path_to_vm=args.path_to_vm,
        action_space="pyautogui",
        screen_size=(args.screen_width, args.screen_height),
        headless=args.headless,
        os_type="Ubuntu",
        # syscity reads the accessibility tree itself, over the VM's HTTP
        # service; shipping one through the harness would only cost time.
        require_a11y_tree=False,
        vm_secret_mounts=args.vm_secret_mount,
    )
    # The service port is part of the VM address the provider hands back.
    agent.port = getattr(env, "server_port", 5000)

    # One model round can carry several tool calls, so replay needs more loop
    # iterations than syscity needs model rounds.
    harness_steps = max(50, args.max_steps * 3)

    scores = []
    result_paths = []
    infra_errors = []
    try:
        for domain, example_id, config_path in examples:
            example_result_dir = os.path.join(args.result_dir, domain, example_id)
            result_file = os.path.join(example_result_dir, "result.txt")
            if args.resume and os.path.isfile(result_file):
                logger.info("skipping %s/%s (already scored)", domain, example_id)
                continue

            with open(config_path, "r", encoding="utf-8") as handle:
                example = json.load(handle)
            os.makedirs(example_result_dir, exist_ok=True)

            agent.task_id = example["id"]
            agent.artifacts_dir = example_result_dir
            logger.info("[%s/%s] %s", domain, example_id, example["instruction"])

            try:
                lib_run_single.run_single_example(
                    agent,
                    env,
                    example,
                    harness_steps,
                    example["instruction"],
                    args,
                    example_result_dir,
                    scores,
                )
                result_paths.append(result_file)
            except SyscityUnavailable:
                raise
            except SyscityRunFailed as error:
                # syscity never got a turn: not a model failure.
                infra_errors.append("{0}/{1}: {2}".format(domain, example_id, error))
                logger.error("infra error on %s/%s: %s", domain, example_id, error)
                lib_run_single.record_example_error(
                    example, domain, example_id, error, env, example_result_dir, args
                )
            except Exception as error:  # noqa: BLE001 - the harness's own per-task boundary
                infra_errors.append(
                    "{0}/{1}: {2}: {3}".format(domain, example_id, type(error).__name__, error)
                )
                logger.error(
                    "exception on %s/%s:\n%s", domain, example_id, traceback.format_exc()
                )
                lib_run_single.record_example_error(
                    example, domain, example_id, error, env, example_result_dir, args
                )
    finally:
        try:
            env.close()
        except Exception:  # noqa: BLE001 - a failed close must not hide the scores
            logger.warning("closing the environment failed:\n%s", traceback.format_exc())

    report = summarize(result_paths, scores, infra_errors)
    report["split"] = args.split if not args.meta else args.meta
    report["domain"] = args.domain
    report["syscity"] = args.syscity
    report["provider"] = args.provider
    report["model"] = args.model
    report["max_steps"] = args.max_steps

    summary_dir = os.path.join(args.result_dir, "summary")
    os.makedirs(summary_dir, exist_ok=True)
    summary_path = os.path.join(summary_dir, "syscity.json")
    with open(summary_path, "w", encoding="utf-8") as handle:
        json.dump(report, handle, indent=2)
        handle.write("\n")

    print(json.dumps(report, indent=2))
    print("summary written to {0}".format(summary_path))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
