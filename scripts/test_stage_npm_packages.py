#!/usr/bin/env python3

import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch


SCRIPT_PATH = Path(__file__).resolve().parent / "stage_npm_packages.py"
SPEC = importlib.util.spec_from_file_location("stage_npm_packages", SCRIPT_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"Unable to load module from {SCRIPT_PATH}")
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ResolveReleaseWorkflowTests(unittest.TestCase):
    def test_queries_upstream_repo_for_release_workflow(self) -> None:
        expected = {
            "workflowName": "rust-release",
            "url": "https://github.com/openai/codex/actions/runs/20345806534",
            "headSha": "5b9d9a60d74c8ee2cb34d60fb14b71990e8318ea",
        }
        with patch.object(
            MODULE.subprocess,
            "check_output",
            return_value=MODULE.json.dumps(expected),
        ) as check_output:
            workflow = MODULE.resolve_release_workflow("0.74.0")

        self.assertEqual(workflow, expected)
        cmd = check_output.call_args.args[0]
        self.assertEqual(cmd[:4], ["gh", "run", "list", "-R"])
        self.assertEqual(cmd[4], MODULE.GITHUB_REPO)
        self.assertIn("--branch", cmd)
        self.assertEqual(cmd[cmd.index("--branch") + 1], "rust-v0.74.0")
        self.assertIn("--workflow", cmd)
        self.assertEqual(cmd[cmd.index("--workflow") + 1], MODULE.WORKFLOW_NAME)

    def test_missing_workflow_error_mentions_repo_and_branch(self) -> None:
        with patch.object(MODULE.subprocess, "check_output", return_value=""):
            with self.assertRaisesRegex(
                RuntimeError,
                r"openai/codex.*rust-v0\.74\.0.*\.github/workflows/rust-release\.yml",
            ):
                MODULE.resolve_release_workflow("0.74.0")


if __name__ == "__main__":
    unittest.main()
