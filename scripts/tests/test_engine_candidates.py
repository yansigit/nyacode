"""Qualification identity, privilege boundaries, and conservative selection."""

import copy
import hashlib
import importlib.util
import io
import json
import os
import subprocess
import tempfile
import unittest
import zipfile
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "engine_candidates", Path(__file__).resolve().parents[1] / "engine_candidates.py"
)
candidates = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(candidates)
BASE = "a" * 40
HEAD = "b" * 40
REVISION = "c" * 40
REPOSITORY = "owner/product"
TAG = "rust-v1.2.3"


def lock():
    return {
        "schema_version": 1,
        "repository": "openai/codex",
        "revision": BASE,
        "release_tag": None,
        "archive_sha256": "d" * 64,
        "toolchain": "1.95.0",
        "integration_contract_version": 1,
        "assembled_lock": "integration/codex/Cargo.lock",
    }


def protection():
    return {
        "requiresStrictStatusChecks": True,
        "requiredStatusChecks": [
            {"context": "Engine acceptance", "app": {"databaseId": 15368}}
        ],
        "isAdminEnforced": True,
        "allowsForcePushes": False,
        "allowsDeletions": False,
        "bypassPullRequestAllowances": {"totalCount": 0},
    }


def qualification():
    run = {
        "id": 123,
        "run_attempt": 2,
        "workflow_id": 456,
        "event": "workflow_dispatch",
        "path": ".github/workflows/engine-qualification.yml",
        "status": "completed",
        "conclusion": "success",
        "head_sha": HEAD,
        "head_branch": f"engine/{TAG}",
        "head_repository": {"full_name": REPOSITORY},
    }
    pr = {
        "number": 7,
        "state": "open",
        "draft": False,
        "head": {
            "sha": HEAD,
            "ref": f"engine/{TAG}",
            "repo": {"full_name": REPOSITORY},
        },
        "base": {"sha": BASE, "ref": "main"},
    }
    report = {
        "schema_version": 1,
        "head_sha": HEAD,
        "base_sha": BASE,
        "pull_request": 7,
        "run_id": 123,
        "run_attempt": 2,
        "release_tag": TAG,
        "results": {
            name: "success"
            for name in (
                "dependency_policy",
                "controls",
                "providers",
                "validate",
                "checks",
                "build",
                "sdk",
                "verify",
            )
        },
    }
    jobs = [
        {"name": name, "head_sha": HEAD, "status": "completed", "conclusion": "success"}
        for name in sorted(candidates.REQUIRED_JOBS)
    ]
    return {
        "run": run,
        "pr": pr,
        "report": report,
        "jobs": jobs,
        "current_base": BASE,
        "workflow_id": 456,
        "repository": REPOSITORY,
        "changed_files": [{"filename": "engine.lock", "status": "modified"}],
        "parents": [BASE],
    }


class SelectionTest(unittest.TestCase):
    def test_engine_candidate_branches_always_require_full_and_reject_repairs(self):
        self.assertEqual(
            candidates.select_mode(["engine.lock"], f"engine/{TAG}"), "full"
        )
        for paths in (
            ["engine.lock", "integration/codex/adapter/src/lib.rs"],
            ["crates/providers/provider-api/src/lib.rs"],
            [],
        ):
            with self.subTest(paths=paths), self.assertRaises(ValueError):
                candidates.select_mode(paths, f"engine/{TAG}")

    def test_provider_changes_include_affected_engine_checks(self):
        self.assertEqual(
            candidates.select_mode(["crates/providers/provider-runtime/src/pool.rs"]),
            "providers",
        )
        self.assertEqual(
            candidates.select_mode(["integration/codex/adapter/src/session.rs"]),
            "full",
        )

    def test_shared_dependencies_engine_changes_and_unknown_paths_require_full_matrix(
        self,
    ):
        for path in (
            "engine.lock",
            "crates/providers/Cargo.toml",
            "crates/providers/Cargo.lock",
            "crates/providers/provider-runtime/Cargo.toml",
            "crates/providers/provider-runtime/build.rs",
            "crates/providers/rust-toolchain.toml",
            "integration/codex/Cargo.lock",
            "integration/codex/patches/core.patch",
            "codex-rs/core/src/lib.rs",
            ".github/workflows/engine-qualification.yml",
            "unexpected.txt",
        ):
            with self.subTest(path=path):
                self.assertEqual(
                    candidates.select_mode(
                        ["crates/providers/provider-api/src/lib.rs", path]
                    ),
                    "full",
                )
        self.assertEqual(candidates.select_mode([]), "full")

    def test_known_prose_and_controller_test_only_require_controls(self):
        self.assertEqual(
            candidates.select_mode(["README.md", "docs/releases.md"]), "controls"
        )


class LockTest(unittest.TestCase):
    def setUp(self):
        self.original = lock()
        self.candidate = {
            **self.original,
            "revision": REVISION,
            "release_tag": TAG,
            "archive_sha256": "e" * 64,
            "toolchain": "1.96.0",
        }

    def test_only_four_pinned_release_fields_can_change(self):
        candidates.validate_lock(self.original, self.candidate, TAG, REVISION)
        for field, value in (
            ("repository", "attacker/repo"),
            ("assembled_lock", "../../payload"),
            ("integration_contract_version", 2),
            ("new_field", "payload"),
        ):
            with self.subTest(field=field), self.assertRaises(ValueError):
                candidates.validate_lock(
                    self.original, {**self.candidate, field: value}, TAG, REVISION
                )

    def test_missing_fields_tag_mismatch_weak_digest_and_toolchain_injection_stop(self):
        for field, value in (
            ("revision", "deadbeef"),
            ("release_tag", "rust-v1.2.3-beta.1"),
            ("archive_sha256", "e" * 63),
            ("toolchain", "stable --command payload"),
        ):
            with self.subTest(field=field), self.assertRaises(ValueError):
                candidates.validate_lock(
                    self.original, {**self.candidate, field: value}, TAG, REVISION
                )
        del self.candidate["schema_version"]
        with self.assertRaises(ValueError):
            candidates.validate_lock(self.original, self.candidate, TAG, REVISION)


class PromotionTest(unittest.TestCase):
    def test_missing_loose_spoofable_or_bypassable_protection_stops(self):
        candidates.validate_protection(protection())
        invalid = [
            {},
            {**protection(), "isAdminEnforced": False},
            {**protection(), "allowsForcePushes": True},
            {
                **protection(),
                "bypassPullRequestAllowances": {"totalCount": 1},
            },
        ]
        loose = protection()
        loose["requiresStrictStatusChecks"] = False
        invalid.append(loose)
        spoofable = protection()
        spoofable["requiredStatusChecks"][0]["app"]["databaseId"] = -1
        invalid.append(spoofable)
        for value in invalid:
            with self.subTest(value=value), self.assertRaises(ValueError):
                candidates.validate_protection(value)

    def test_exact_complete_qualification_is_eligible(self):
        candidates.validate_promotion(**qualification())

    def test_stale_or_foreign_run_cannot_promote(self):
        for field, value in (
            ("head_sha", BASE),
            ("workflow_id", 999),
            ("run_attempt", 1),
            ("event", "pull_request"),
            ("path", ".github/workflows/attacker.yml"),
            ("conclusion", "failure"),
            ("status", "in_progress"),
            ("head_repository", {"full_name": "attacker/fork"}),
        ):
            fixture = qualification()
            fixture["run"][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                candidates.validate_promotion(**fixture)

    def test_stale_base_extra_commits_and_fork_pr_stop(self):
        for mutate in (
            lambda f: f.update(current_base=REVISION),
            lambda f: f.update(parents=[BASE, REVISION]),
            lambda f: f["pr"]["head"].update(repo={"full_name": "attacker/fork"}),
            lambda f: f["pr"]["head"].update(ref="unrelated"),
            lambda f: f["pr"].update(draft=True),
            lambda f: f["pr"].update(state="closed"),
        ):
            fixture = qualification()
            mutate(fixture)
            with self.assertRaises(ValueError):
                candidates.validate_promotion(**fixture)

    def test_missing_failed_cancelled_skipped_or_stale_jobs_stop(self):
        for conclusion in ("failure", "cancelled", "skipped", "neutral", None):
            fixture = qualification()
            fixture["jobs"][0]["conclusion"] = conclusion
            with self.subTest(conclusion=conclusion), self.assertRaises(ValueError):
                candidates.validate_promotion(**fixture)
        for mutate in (
            lambda f: f["jobs"].pop(),
            lambda f: f["jobs"].append(copy.deepcopy(f["jobs"][0])),
            lambda f: f["jobs"][0].update(head_sha=BASE),
            lambda f: f["jobs"][0].update(status="queued"),
        ):
            fixture = qualification()
            mutate(fixture)
            with self.assertRaises(ValueError):
                candidates.validate_promotion(**fixture)

    def test_artifact_cannot_claim_another_run_attempt_base_head_or_pr(self):
        for field, value in (
            ("run_id", 124),
            ("run_attempt", 1),
            ("head_sha", BASE),
            ("base_sha", REVISION),
            ("pull_request", 8),
            ("results", {"build": "success"}),
            ("release_tag", "rust-v1.2.3-beta.1"),
        ):
            fixture = qualification()
            fixture["report"][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                candidates.validate_promotion(**fixture)

    def test_adapter_tests_workflows_generated_files_and_renames_are_not_auto_approved(
        self,
    ):
        for path in (
            "integration/codex/adapter/src/lib.rs",
            "scripts/tests/test_engine_candidates.py",
            ".github/workflows/engine-qualification.yml",
            "integration/codex/Cargo.lock",
        ):
            fixture = qualification()
            fixture["changed_files"].append({"filename": path, "status": "modified"})
            with self.subTest(path=path), self.assertRaises(ValueError):
                candidates.validate_promotion(**fixture)
        fixture = qualification()
        fixture["changed_files"][0]["status"] = "renamed"
        with self.assertRaises(ValueError):
            candidates.validate_promotion(**fixture)


class GitHubBoundaryTest(unittest.TestCase):
    def test_manual_replay_validates_run_id_before_any_github_request(self):
        for value in ("", "0", "-1", "+123", "1.5", "true", "123\n456"):
            with (
                self.subTest(value=value),
                patch.object(
                    candidates.sys, "argv", ["controller", "observe", "--run-id", value]
                ),
                patch.object(candidates.sys, "stderr", io.StringIO()),
                patch.object(candidates, "api") as api,
                self.assertRaises(SystemExit) as error,
            ):
                candidates.main()
            self.assertEqual(error.exception.code, 2)
            api.assert_not_called()

    def test_manual_replay_invokes_only_observation_for_the_requested_run(self):
        with (
            patch.object(
                candidates.sys,
                "argv",
                ["controller", "observe", "--run-id", "123", "--wait-seconds", "21000"],
            ),
            patch.dict(os.environ, {"GITHUB_REPOSITORY": REPOSITORY}),
            patch.object(candidates, "api", return_value={"default_branch": "main"}),
            patch.object(candidates, "observe") as observe,
            patch.object(candidates, "discover") as discover,
            patch.object(candidates, "publish") as publish,
            patch.object(candidates, "promote") as promote,
        ):
            self.assertEqual(candidates.main(), 0)
            args, repository, branch = observe.call_args.args
            self.assertEqual((args.run_id, args.wait_seconds), (123, 21000))
            self.assertEqual((repository, branch), (REPOSITORY, "main"))
            discover.assert_not_called()
            publish.assert_not_called()
            promote.assert_not_called()

    def test_prerelease_and_draft_releases_are_ineligible(self):
        for flag in ("draft", "prerelease"):
            response = {
                "draft": False,
                "prerelease": False,
                "tag_name": TAG,
                flag: True,
            }
            with (
                patch.object(candidates, "api", return_value=response),
                self.assertRaises(ValueError),
            ):
                candidates.stable_release(TAG, "openai/codex")

    def test_annotated_tag_resolves_to_full_commit(self):
        with patch.object(
            candidates,
            "api",
            side_effect=[
                {"draft": False, "prerelease": False, "tag_name": TAG},
                {"object": {"type": "tag", "sha": HEAD}},
                {"object": {"type": "commit", "sha": REVISION}},
            ],
        ):
            self.assertEqual(candidates.stable_release(TAG, "openai/codex"), REVISION)

    def test_artifacts_are_read_as_bounded_json_without_extraction(self):
        for name, content in (
            ("qualification.json", "{}"),
            ("../payload.py", "print('payload')"),
            ("qualification.json", " " * 65537),
        ):
            archive = io.BytesIO()
            with zipfile.ZipFile(archive, "w") as stream:
                stream.writestr(name, content)
            with (
                patch.object(
                    candidates,
                    "pages",
                    return_value=[
                        {"id": 1, "name": "engine-qualification", "expired": False}
                    ],
                ),
                patch.object(candidates, "command", return_value=archive.getvalue()),
            ):
                if name == "qualification.json" and len(content) < 65536:
                    self.assertEqual(candidates.report_for(REPOSITORY, 123), {})
                else:
                    with self.assertRaises(ValueError):
                        candidates.report_for(REPOSITORY, 123)

    def test_publish_rejects_stale_proposal_before_any_github_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "candidate.json"
            path.write_text(json.dumps({"schema_version": 1, "base_sha": HEAD}))
            args = type("Args", (), {"proposal": path})()
            with (
                patch.object(candidates, "current_base", return_value=BASE),
                patch.object(candidates, "api") as api,
                self.assertRaises(ValueError),
            ):
                candidates.publish(args, REPOSITORY, "main")
            api.assert_not_called()

    def test_github_commands_use_structured_stdin_and_no_shell(self):
        payload = {"body": "literal $(do-not-run) `anything`\nsecond line"}
        with patch.object(
            candidates.subprocess,
            "run",
            return_value=subprocess.CompletedProcess([], 0, b"{}"),
        ) as run:
            candidates.api("repos/owner/repo/git/commits", data=payload)
            self.assertEqual(json.loads(run.call_args.kwargs["input"]), payload)
            self.assertNotIn("shell", run.call_args.kwargs)
            self.assertEqual(run.call_args.args[0][0:2], ["gh", "api"])


class WorkflowBoundaryTest(unittest.TestCase):
    def test_manual_and_fallback_observation_survive_skipped_publication(self):
        workflow = (
            candidates.ROOT / ".github/workflows/engine-upgrades.yml"
        ).read_text()
        discovery = workflow.split("\n  discover:\n", 1)[1].split(
            "\n  publish-candidate:\n", 1
        )[0]
        observer = workflow.split("\n  observe:\n", 1)[1].split("\n  promote:\n", 1)[0]
        self.assertIn(
            "if: github.event_name != 'workflow_run' && inputs.qualification_run == ''",
            discovery,
        )
        self.assertIn("needs: publish-candidate", observer)
        # A status function overrides GitHub's implicit success() after a skipped need.
        self.assertIn("!cancelled() &&", observer)
        self.assertIn(
            "github.event_name == 'workflow_run' || inputs.qualification_run != '' ||",
            observer,
        )
        self.assertIn(
            "needs.publish-candidate.outputs.qualification_run != ''", observer
        )
        self.assertIn("timeout-minutes: 360", observer)
        self.assertIn("--wait-seconds 21000", observer)

    def test_continuation_keeps_trusted_checkouts_and_separate_promotion_permissions(
        self,
    ):
        workflow = (
            candidates.ROOT / ".github/workflows/engine-upgrades.yml"
        ).read_text()
        observer = workflow.split("\n  observe:\n", 1)[1].split("\n  promote:\n", 1)[0]
        promoter = workflow.split("\n  promote:\n", 1)[1]
        self.assertIn(
            "contents: read\n      pull-requests: write\n      actions: read", observer
        )
        self.assertNotIn("contents: write", observer)
        self.assertNotIn("actions: write", observer)
        for job in (observer, promoter):
            self.assertIn("ref: ${{ github.event.repository.default_branch }}", job)
            self.assertIn("persist-credentials: false", job)
        self.assertIn(
            "QUALIFICATION_RUN: ${{ needs.observe.outputs.qualification_run }}",
            promoter,
        )
        self.assertIn(
            "if: needs.observe.outputs.consider_promotion == 'true'", promoter
        )
        qualification_workflow = (
            candidates.ROOT / ".github/workflows/engine-qualification.yml"
        ).read_text()
        self.assertIn("permissions:\n  contents: read", qualification_workflow)
        self.assertNotIn("actions: write", qualification_workflow)


class ControllerFlowTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.original = lock()
        self.candidate = {**self.original, "revision": REVISION, "release_tag": TAG}
        self.proposal = self.root / "candidate.json"
        self.proposal.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "base_sha": BASE,
                    "tag": TAG,
                    "revision": REVISION,
                    "engine_lock": self.candidate,
                    "prepared": True,
                }
            )
        )

    def test_disabled_schedule_does_not_query_releases_or_run_candidate_code(self):
        (self.root / "engine.lock").write_text(json.dumps(self.original))
        with (
            patch.object(candidates, "ROOT", self.root),
            patch.object(candidates, "current_base", return_value=BASE),
            patch.object(candidates.subprocess, "check_output", return_value=BASE),
            patch.dict(os.environ, {"NYACODE_ENGINE_UPGRADES_ENABLED": ""}),
            patch.object(candidates, "pages") as pages,
            patch.object(candidates.subprocess, "run") as run,
        ):
            candidates.discover(SimpleNamespace(scheduled=True), REPOSITORY, "main")
            pages.assert_not_called()
            run.assert_not_called()

    def test_publish_uses_git_data_api_and_explicit_readonly_qualification_dispatch(
        self,
    ):
        calls = []
        output = self.root / "github-output"

        def api(path, *, data=None):
            calls.append((path, data))
            if path.endswith("per_page=100"):
                return []
            if "/git/ref/heads/" in path:
                raise subprocess.CalledProcessError(1, ["gh"], stderr=b"HTTP 404")
            if path.endswith(BASE):
                return {"tree": {"sha": "tree"}}
            if path.endswith("/pulls"):
                return {"number": 7, "html_url": "https://example.invalid/pr/7"}
            if path.endswith("/dispatches"):
                return {"workflow_run_id": 789}
            return {"sha": HEAD}

        with (
            patch.object(candidates, "current_base", return_value=BASE),
            patch.object(candidates, "lock_at", return_value=self.original),
            patch.object(candidates, "stable_release", return_value=REVISION),
            patch.object(candidates, "api", side_effect=api),
            patch.object(candidates.subprocess, "run") as run,
        ):
            candidates.publish(
                SimpleNamespace(proposal=self.proposal, github_output=output),
                REPOSITORY,
                "main",
            )
            run.assert_not_called()
            self.assertEqual(output.read_text(), "qualification_run=789\n")
            dispatch_path, dispatch = calls[-1]
            self.assertEqual(
                dispatch_path,
                f"repos/{REPOSITORY}/actions/workflows/{candidates.WORKFLOW}/dispatches",
            )
            self.assertEqual(
                dispatch,
                {
                    "ref": f"engine/{TAG}",
                    "inputs": {
                        "base_sha": BASE,
                        "pull_request": "7",
                        "release_tag": TAG,
                    },
                    "return_run_details": True,
                },
            )
            tree = next(data for path, data in calls if path.endswith("/git/trees"))
            self.assertEqual([entry["path"] for entry in tree["tree"]], ["engine.lock"])
            blob = next(data for path, data in calls if path.endswith("/git/blobs"))
            self.assertEqual(json.loads(blob["content"]), self.candidate)
            pr = next(data for path, data in calls if path.endswith("/pulls"))
            self.assertFalse(pr["draft"])

    def test_publish_never_exports_an_invalid_dispatch_run_id(self):
        output = self.root / "github-output"
        for response in (
            None,
            {},
            {"workflow_run_id": True},
            {"workflow_run_id": "123"},
            {"workflow_run_id": 0},
            {"workflow_run_id": -1},
        ):
            with (
                self.subTest(response=response),
                patch.object(candidates, "current_base", return_value=BASE),
                patch.object(candidates, "lock_at", return_value=self.original),
                patch.object(candidates, "stable_release", return_value=REVISION),
                patch.object(
                    candidates,
                    "api",
                    side_effect=[
                        [],
                        subprocess.CalledProcessError(1, ["gh"], stderr=b"HTTP 404"),
                        {"tree": {"sha": "tree"}},
                        {"sha": "blob"},
                        {"sha": "tree"},
                        {"sha": HEAD},
                        {},
                        {"number": 7, "html_url": "https://example.invalid/pr/7"},
                        response,
                    ],
                ),
                self.assertRaisesRegex(ValueError, "valid workflow run ID"),
            ):
                candidates.publish(
                    SimpleNamespace(proposal=self.proposal, github_output=output),
                    REPOSITORY,
                    "main",
                )
            self.assertFalse(output.exists())

    def test_failed_preflight_retains_valid_candidate_and_accepted_pin(self):
        (self.root / "engine.lock").write_text(json.dumps(self.original))
        proposal_dir = self.root / ".engine/upgrades" / TAG
        proposal_dir.mkdir(parents=True)
        (proposal_dir / "engine.lock").write_text(json.dumps(self.candidate))
        with (
            patch.object(candidates, "ROOT", self.root),
            patch.object(candidates, "current_base", return_value=BASE),
            patch.object(candidates.subprocess, "check_output", return_value=BASE),
            patch.object(candidates, "stable_release", return_value=REVISION),
            patch.object(candidates, "pages", return_value=[]),
            patch.object(candidates, "api", return_value={"status": "ahead"}),
            patch.dict(os.environ, {"GITHUB_RUN_ID": "321"}),
            patch.object(
                candidates.subprocess,
                "run",
                return_value=subprocess.CompletedProcess([], 1),
            ) as run,
        ):
            candidates.discover(
                SimpleNamespace(tag=TAG, scheduled=False, output=self.proposal),
                REPOSITORY,
                "main",
            )
            proposal = json.loads(self.proposal.read_text())
            self.assertFalse(proposal["prepared"])
            self.assertEqual(proposal["engine_lock"], self.candidate)
            self.assertEqual(
                proposal["diagnostics_url"],
                f"https://github.com/{REPOSITORY}/actions/runs/321",
            )
            self.assertEqual(
                json.loads((self.root / "engine.lock").read_text()), self.original
            )
            self.assertFalse(run.call_args.kwargs["check"])
            self.proposal.unlink()
            (proposal_dir / "engine.lock").write_text(
                json.dumps({**self.candidate, "repository": "attacker/engine"})
            )
            with self.assertRaisesRegex(ValueError, "integration contract"):
                candidates.discover(
                    SimpleNamespace(tag=TAG, scheduled=False, output=self.proposal),
                    REPOSITORY,
                    "main",
                )
            self.assertFalse(self.proposal.exists())

    def test_failed_preflight_publishes_draft_with_trusted_diagnostics(self):
        proposal = json.loads(self.proposal.read_text())
        proposal.update(prepared=False, diagnostics_url="https://attacker.invalid/")
        self.proposal.write_text(json.dumps(proposal))
        calls = []

        def api(path, *, data=None):
            calls.append((path, data))
            if "/pulls?" in path:
                return []
            if "/git/ref/" in path:
                raise subprocess.CalledProcessError(1, ["gh"], stderr=b"HTTP 404")
            if path.endswith(BASE):
                return {"tree": {"sha": "tree"}}
            if path.endswith("/pulls"):
                return {"number": 7, "html_url": "https://example.invalid/pr/7"}
            if path.endswith("/dispatches"):
                return {"workflow_run_id": 789}
            return {"sha": HEAD}

        with (
            patch.object(candidates, "current_base", return_value=BASE),
            patch.object(candidates, "lock_at", return_value=self.original),
            patch.object(candidates, "stable_release", return_value=REVISION),
            patch.object(candidates, "api", side_effect=api),
            patch.dict(os.environ, {"GITHUB_RUN_ID": "321"}),
            patch.object(candidates.subprocess, "run") as run,
        ):
            candidates.publish(
                SimpleNamespace(proposal=self.proposal), REPOSITORY, "main"
            )
            pr = next(data for path, data in calls if path.endswith("/pulls"))
            self.assertTrue(pr["draft"])
            self.assertIn("Preflight failed", pr["body"])
            self.assertIn(
                f"https://github.com/{REPOSITORY}/actions/runs/321", pr["body"]
            )
            self.assertNotIn("attacker", pr["body"])
            run.assert_not_called()

    def test_existing_candidate_reuses_branch_without_rewriting_it(self):
        proposal = json.loads(self.proposal.read_text())
        proposal["prepared"] = False
        self.proposal.write_text(json.dumps(proposal))
        writes = []

        def api(path, *, data=None):
            if data is not None:
                writes.append(data)
            if "/pulls?" in path:
                return [
                    {
                        "number": 7,
                        "state": "open",
                        "draft": True,
                        "html_url": "https://example.invalid/pr/7",
                    }
                ]
            if "/git/ref/" in path:
                return {"object": {"sha": HEAD}}
            return {
                "total_commits": 1,
                "files": [{"filename": "engine.lock", "status": "modified"}],
            }

        with (
            patch.object(candidates, "current_base", return_value=BASE),
            patch.object(
                candidates, "lock_at", side_effect=[self.original, self.candidate] * 2
            ),
            patch.object(candidates, "stable_release", return_value=REVISION),
            patch.object(candidates, "api", side_effect=api),
            patch.object(candidates.subprocess, "run") as run,
        ):
            candidates.publish(
                SimpleNamespace(proposal=self.proposal), REPOSITORY, "main"
            )
            candidates.publish(
                SimpleNamespace(proposal=self.proposal), REPOSITORY, "main"
            )
            self.assertFalse(writes)
            run.assert_not_called()

    def test_observer_replaces_report_and_preserves_ready_success_candidate(self):
        fixture = qualification()
        body = (
            "Candidate details.\n\n<!-- engine-qualification:start -->"
            "Old report<!-- engine-qualification:end -->"
        )
        output = self.root / "github-output"

        def api(path, *, data=None):
            if "/runs/" in path:
                return fixture["run"]
            if "/workflows/" in path:
                return {"id": 456}
            if "/pulls?" in path:
                return [fixture["pr"]]
            return {**fixture["pr"], "body": body}

        with (
            patch.object(candidates, "api", side_effect=api),
            patch.object(candidates, "run_jobs", return_value=fixture["jobs"]),
            patch.object(candidates, "report_for", return_value=fixture["report"]),
            patch.object(candidates, "command") as command,
            patch.object(candidates.subprocess, "run") as run,
        ):
            candidates.observe(
                SimpleNamespace(run_id=123, github_output=output), REPOSITORY, "main"
            )
            run.assert_not_called()
            report = command.call_args.kwargs["data"]["body"]
            self.assertIn("Candidate details.", report)
            self.assertNotIn("Old report", report)
            self.assertEqual(report.count("<!-- engine-qualification:start -->"), 1)
            self.assertIn("consider_promotion=true", output.read_text())
            self.assertIn("qualification_run=123\n", output.read_text())

    def test_observer_waits_for_the_exact_run_and_reports_its_failed_attempt(self):
        fixture = qualification()
        fixture["run"]["conclusion"] = "failure"
        fixture["jobs"][0]["conclusion"] = "failure"
        output = self.root / "github-output"
        runs = iter(
            [
                {**fixture["run"], "status": status, "conclusion": None}
                for status in ("queued", "in_progress")
            ]
            + [fixture["run"]]
        )

        def api(path, *, data=None):
            if "/runs/" in path:
                self.assertEqual(path, f"repos/{REPOSITORY}/actions/runs/123")
                return next(runs)
            if "/workflows/" in path:
                return {"id": 456}
            if "/pulls?" in path:
                return [fixture["pr"]]
            return {**fixture["pr"], "body": "Candidate details."}

        with (
            patch.object(candidates, "api", side_effect=api),
            patch.object(candidates.time, "monotonic", side_effect=[100, 100, 130]),
            patch.object(candidates.time, "sleep") as sleep,
            patch.object(candidates, "run_jobs", return_value=fixture["jobs"]),
            patch.object(candidates, "report_for") as artifact,
            patch.object(candidates, "command") as command,
            patch.object(candidates.subprocess, "run"),
        ):
            candidates.observe(
                SimpleNamespace(run_id=123, wait_seconds=90, github_output=output),
                REPOSITORY,
                "main",
            )
            self.assertEqual(
                [call.args for call in sleep.call_args_list], [(30,), (30,)]
            )
            artifact.assert_not_called()
            self.assertIn(
                "Qualification **failure**", command.call_args.kwargs["data"]["body"]
            )
            self.assertEqual(
                output.read_text(), "qualification_run=123\nconsider_promotion=false\n"
            )

    def test_observer_wait_expires_without_reporting_or_exporting_promotion(self):
        run = {**qualification()["run"], "status": "in_progress", "conclusion": None}
        output = self.root / "github-output"
        with (
            patch.object(candidates, "api", side_effect=[{"id": 456}, run, run, run]),
            patch.object(
                candidates.time, "monotonic", side_effect=[100, 100, 130, 145]
            ),
            patch.object(candidates.time, "sleep") as sleep,
            patch.object(candidates, "command") as command,
            patch.object(candidates, "run_jobs") as jobs,
            patch.object(candidates.subprocess, "run") as mutation,
            self.assertRaisesRegex(
                ValueError, "replay Engine upgrades with qualification_run"
            ),
        ):
            candidates.observe(
                SimpleNamespace(run_id=123, wait_seconds=45, github_output=output),
                REPOSITORY,
                "main",
            )
        self.assertEqual([call.args for call in sleep.call_args_list], [(30,), (15,)])
        command.assert_not_called()
        jobs.assert_not_called()
        mutation.assert_not_called()
        self.assertFalse(output.exists())

    def test_observer_rejects_foreign_identity_before_waiting(self):
        for field, value in (
            ("id", 999),
            ("workflow_id", 999),
            ("event", "push"),
            ("path", ".github/workflows/untrusted.yml"),
            ("head_repository", {"full_name": "attacker/product"}),
            ("head_sha", "invalid"),
            ("head_branch", "engine/not-a-release"),
        ):
            run = {**qualification()["run"], "status": "in_progress", field: value}
            with (
                self.subTest(field=field),
                patch.object(candidates, "api", side_effect=[{"id": 456}, run]),
                patch.object(candidates.time, "sleep") as sleep,
                patch.object(candidates, "command") as command,
                patch.object(candidates.subprocess, "run") as mutation,
            ):
                candidates.observe(
                    SimpleNamespace(run_id=123, wait_seconds=90, github_output=None),
                    REPOSITORY,
                    "main",
                )
                sleep.assert_not_called()
                command.assert_not_called()
                mutation.assert_not_called()

    def test_observer_reports_failure_cancel_and_missing_artifact_as_draft(self):
        for conclusion in ("failure", "cancelled", "success"):
            fixture = qualification()
            fixture["run"]["conclusion"] = conclusion
            output = self.root / "github-output"
            output.unlink(missing_ok=True)

            def api(path, *, data=None, fixture=fixture):
                if "/runs/" in path:
                    return fixture["run"]
                if "/workflows/" in path:
                    return {"id": 456}
                if "/pulls?" in path:
                    return [fixture["pr"]]
                return {**fixture["pr"], "body": "Original candidate details."}

            with (
                self.subTest(conclusion=conclusion),
                patch.object(candidates, "api", side_effect=api),
                patch.object(candidates, "run_jobs", return_value=fixture["jobs"]),
                patch.object(
                    candidates, "report_for", side_effect=ValueError("missing")
                ),
                patch.object(candidates, "command") as command,
                patch.object(candidates.subprocess, "run") as run,
            ):
                candidates.observe(
                    SimpleNamespace(run_id=123, github_output=output),
                    REPOSITORY,
                    "main",
                )
                self.assertEqual(
                    run.call_args.args[0][:5], ["gh", "pr", "ready", "7", "--undo"]
                )
                body = command.call_args.kwargs["data"]["body"]
                self.assertIn("Original candidate details.", body)
                self.assertIn(f"Qualification **{conclusion}**", body)
                self.assertIn(f"`{HEAD}`", body)
                self.assertIn("/actions/runs/123/attempts/2", body)
                self.assertIn("consider_promotion=false", output.read_text())

    def test_observer_never_writes_report_to_a_different_or_moved_head(self):
        for moved in (False, True):
            fixture = qualification()
            other = copy.deepcopy(fixture["pr"])
            other["head"]["sha"] = REVISION

            def api(path, *, data=None, fixture=fixture, moved=moved, other=other):
                if "/runs/" in path:
                    return fixture["run"]
                if "/workflows/" in path:
                    return {"id": 456}
                if "/pulls?" in path:
                    return [fixture["pr"] if moved else other]
                return other

            with (
                self.subTest(moved=moved),
                patch.object(candidates, "api", side_effect=api),
                patch.object(candidates, "run_jobs", return_value=fixture["jobs"]),
                patch.object(candidates, "report_for", return_value=fixture["report"]),
                patch.object(candidates, "command") as command,
                patch.object(candidates.subprocess, "run") as run,
            ):
                candidates.observe(
                    SimpleNamespace(run_id=123, github_output=None), REPOSITORY, "main"
                )
                command.assert_not_called()
                run.assert_not_called()

    def test_promoter_merges_only_exact_qualified_head_and_stops_on_late_base_move(
        self,
    ):
        for moved, merged in ((False, True), (True, True), (False, False)):
            fixture = qualification()
            fixture["report"]["engine_lock_sha256"] = hashlib.sha256(
                (json.dumps(self.candidate, indent=2) + "\n").encode()
            ).hexdigest()

            def api(path, *, data=None, snapshot=fixture):
                if path.endswith("/runs/123"):
                    return snapshot["run"]
                if "/workflows/" in path:
                    return {"id": 456}
                if path == "graphql":
                    return {
                        "data": {
                            "repository": {
                                "ref": {"branchProtectionRule": protection()}
                            }
                        }
                    }
                if path.endswith("/pulls/7"):
                    return {
                        **snapshot["pr"],
                        "mergeable_state": "clean",
                    }
                return {"parents": [{"sha": BASE}]}

            with (
                self.subTest(moved=moved, merged=merged),
                patch.object(candidates, "api", side_effect=api),
                patch.object(candidates, "report_for", return_value=fixture["report"]),
                patch.object(
                    candidates,
                    "current_base",
                    side_effect=[BASE, HEAD if moved else BASE],
                ),
                patch.object(
                    candidates, "pages", return_value=fixture["changed_files"]
                ),
                patch.object(candidates, "run_jobs", return_value=fixture["jobs"]),
                patch.object(
                    candidates, "lock_at", side_effect=[self.original, self.candidate]
                ),
                patch.object(candidates, "stable_release", return_value=REVISION),
                patch.object(
                    candidates, "command", return_value={"merged": merged}
                ) as command,
            ):
                if moved:
                    with self.assertRaisesRegex(ValueError, "moved"):
                        candidates.promote(
                            SimpleNamespace(run_id=123), REPOSITORY, "main"
                        )
                    command.assert_not_called()
                elif not merged:
                    with self.assertRaisesRegex(
                        ValueError, "did not immediately merge"
                    ):
                        candidates.promote(
                            SimpleNamespace(run_id=123), REPOSITORY, "main"
                        )
                else:
                    candidates.promote(SimpleNamespace(run_id=123), REPOSITORY, "main")
                    command.assert_called_once_with(
                        [
                            "gh",
                            "api",
                            f"repos/{REPOSITORY}/pulls/7/merge",
                            "--method",
                            "PUT",
                            "--input",
                            "-",
                        ],
                        data={"sha": HEAD, "merge_method": "squash"},
                    )


if __name__ == "__main__":
    unittest.main()
