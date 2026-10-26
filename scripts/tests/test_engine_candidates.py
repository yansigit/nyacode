"""Qualification identity, privilege boundaries, and conservative selection."""

import contextlib
import copy
import hashlib
import importlib.util
import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
import zipfile
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import call, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
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
NEW = "e" * 40
OLD = "d" * 40
PR_URL = "https://example.invalid/pr/7"


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
        "classification": "mechanical",
        "results": {
            name: "success"
            for name in (
                "dependency_policy",
                "container",
                "controls",
                "providers",
                "validate",
                "checks",
                "build",
                "sdk",
                "generated",
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


def proposal(**overrides):
    """A schema 2 candidate proposal, as the read-only construction job writes it."""
    value = {
        "schema_version": 2,
        "base_sha": BASE,
        "tag": TAG,
        "revision": REVISION,
        "commit": HEAD,
        "resolution": "none",
        "prepared": True,
        "conflicts": [],
        "bundle": "candidate.bundle",
    }
    value.update(overrides)
    return value


def run_publish(
    api,
    proposal_path,
    bundle_path,
    *,
    original,
    candidate,
    commit=HEAD,
    verify=(),
    output=None,
):
    """Run publish with GitHub and bundle boundaries mocked; returns the started mocks."""
    mocks = {
        "current_base": patch.object(candidates, "current_base", return_value=BASE),
        "lock_at": patch.object(candidates, "lock_at", return_value=original),
        "candidate_lock_at": patch.object(
            candidates, "candidate_lock_at", return_value=candidate
        ),
        "import_candidate_bundle": patch.object(
            candidates.engine_sync, "import_candidate_bundle", return_value=commit
        ),
        "verify_candidate_commit": patch.object(
            candidates.engine_sync, "verify_candidate_commit", return_value=list(verify)
        ),
        "stable_release": patch.object(
            candidates, "stable_release", return_value=REVISION
        ),
        "api": patch.object(candidates, "api", side_effect=api),
        "pages": patch.object(candidates, "pages", return_value=[]),
        "push_candidate": patch.object(candidates, "push_candidate"),
        "subprocess_run": patch.object(candidates.subprocess, "run"),
    }
    with contextlib.ExitStack() as stack:
        started = {name: stack.enter_context(mock) for name, mock in mocks.items()}
        candidates.publish(
            SimpleNamespace(
                proposal=proposal_path,
                bundle=bundle_path,
                github_output=output,
            ),
            REPOSITORY,
            "main",
        )
    started["subprocess_run"].assert_not_called()
    return started


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
            lambda f: f["pr"].update(labels=[{"name": "engine:reject"}]),
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

    def test_full_workspace_container_is_required_in_jobs_and_report(self):
        name = "Full workspace container / Complete Linux container verification"
        for missing in ("job", "report"):
            fixture = qualification()
            if missing == "job":
                fixture["jobs"] = [
                    job for job in fixture["jobs"] if job["name"] != name
                ]
            else:
                del fixture["report"]["results"]["container"]
            with self.subTest(missing=missing), self.assertRaises(ValueError):
                candidates.validate_promotion(**fixture)
        for conclusion in ("failure", "cancelled", "skipped"):
            fixture = qualification()
            next(job for job in fixture["jobs"] if job["name"] == name)[
                "conclusion"
            ] = conclusion
            with self.subTest(conclusion=conclusion), self.assertRaises(ValueError):
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
            "integration/codex/overlays/note.txt",
        ):
            fixture = qualification()
            fixture["changed_files"].append({"filename": path, "status": "modified"})
            with self.subTest(path=path), self.assertRaises(ValueError):
                candidates.validate_promotion(**fixture)
        fixture = qualification()
        fixture["changed_files"][0]["status"] = "renamed"
        with self.assertRaises(ValueError):
            candidates.validate_promotion(**fixture)
        fixture = qualification()
        fixture["changed_files"].append(
            {"filename": "integration/codex/Cargo.lock", "status": "modified"}
        )
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
                patch.dict(os.environ, {"NYACODE_ENGINE_UPGRADES_ENABLED": "true"}),
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
        self.proposal.write_text(json.dumps(proposal()))
        self.bundle = self.root / "candidate.bundle"
        self.bundle.write_bytes(b"bundle")
        for patcher in (
            patch.dict(os.environ, {"NYACODE_ENGINE_UPGRADES_ENABLED": "true"}),
            patch.object(candidates, "pages", return_value=[]),
        ):
            patcher.start()
            self.addCleanup(patcher.stop)

    def test_disabled_discovery_in_every_mode_does_not_query_releases_or_run_code(
        self,
    ):
        (self.root / "engine.lock").write_text(json.dumps(self.original))
        with (
            patch.object(candidates, "ROOT", self.root),
            patch.object(candidates, "current_base", return_value=BASE),
            patch.object(candidates.subprocess, "check_output", return_value=BASE),
            patch.dict(os.environ, {"NYACODE_ENGINE_UPGRADES_ENABLED": ""}),
            patch.object(candidates, "pages") as pages,
            patch.object(candidates.subprocess, "run") as run,
        ):
            for tag in ("", TAG):
                with self.subTest(tag=tag):
                    candidates.discover(
                        SimpleNamespace(tag=tag, output=self.proposal),
                        REPOSITORY,
                        "main",
                    )
            pages.assert_not_called()
            run.assert_not_called()

    def test_publish_pushes_the_verified_bundle_and_dispatches_readonly_qualification(
        self,
    ):
        calls = []
        output = self.root / "github-output"

        def api(path, *, data=None):
            calls.append((path, data))
            if path.endswith("per_page=100"):
                return []
            if "/git/ref/heads/" in path:
                raise candidates.GhError(1, ["gh"], b"HTTP 404")
            if path.endswith("/pulls"):
                return {"number": 7, "html_url": "https://example.invalid/pr/7"}
            if path.endswith("/dispatches"):
                return {"workflow_run_id": 789}
            raise AssertionError(path)

        started = run_publish(
            api,
            self.proposal,
            self.bundle,
            original=self.original,
            candidate=self.candidate,
            output=output,
        )
        started["import_candidate_bundle"].assert_called_once_with(
            candidates.ROOT, self.bundle
        )
        started["verify_candidate_commit"].assert_called_once_with(
            candidates.ROOT, HEAD, BASE, TAG
        )
        started["push_candidate"].assert_called_once_with(
            REPOSITORY, HEAD, f"engine/{TAG}", force=False
        )
        self.assertEqual(output.read_text(), "qualification_run=789\n")
        dispatch = next(data for path, data in calls if path.endswith("/dispatches"))
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
        pr = next(data for path, data in calls if path.endswith("/pulls"))
        self.assertFalse(pr["draft"])

    def test_publish_refuses_schema_one_and_bundles_outside_the_allowlist(self):
        legacy = self.root / "legacy.json"
        legacy.write_text(
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
        with (
            patch.object(candidates, "current_base", return_value=BASE),
            patch.object(candidates, "api") as api,
            patch.object(candidates.engine_sync, "import_candidate_bundle") as imported,
        ):
            with self.assertRaisesRegex(ValueError, "Unsupported candidate proposal"):
                candidates.publish(
                    SimpleNamespace(proposal=legacy, bundle=self.bundle),
                    REPOSITORY,
                    "main",
                )
            imported.assert_not_called()
            api.assert_not_called()
        problem = "path outside the candidate allowlist: .github/workflows/x.yml"
        with self.assertRaisesRegex(ValueError, "not allowlisted"):
            run_publish(
                lambda path, *, data=None: {},
                self.proposal,
                self.bundle,
                original=self.original,
                candidate=self.candidate,
                verify=[problem],
            )

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
            replies = iter(
                [
                    [],
                    candidates.GhError(1, ["gh"], b"HTTP 404"),
                    {"number": 7, "html_url": "https://example.invalid/pr/7"},
                    response,
                ]
            )

            def api(path, *, data=None, replies=replies):
                reply = next(replies)
                if isinstance(reply, Exception):
                    raise reply
                return reply

            with (
                self.subTest(response=response),
                self.assertRaisesRegex(ValueError, "valid workflow run ID"),
            ):
                run_publish(
                    api,
                    self.proposal,
                    self.bundle,
                    original=self.original,
                    candidate=self.candidate,
                    output=output,
                )
            self.assertFalse(output.exists())

    def test_discovery_plans_the_release_without_running_code_or_writing_the_lock(
        self,
    ):
        (self.root / "engine.lock").write_text(json.dumps(self.original))
        plan_value = {
            "schema_version": 1,
            "action": "sync",
            "main_sha": BASE,
            "from": {"kind": "main", "ref": BASE},
            "reason": "no snapshot",
            "candidate": {"tag": TAG, "revision": REVISION},
        }
        output = self.root / "plan.json"
        with (
            patch.object(candidates, "ROOT", self.root),
            patch.object(candidates, "current_base", return_value=BASE),
            patch.object(candidates.subprocess, "check_output", return_value=BASE),
            patch.object(candidates, "stable_release", return_value=REVISION),
            patch.object(candidates, "api", return_value={"status": "ahead"}),
            patch.object(
                candidates.engine_sync, "plan", return_value=plan_value
            ) as plan,
            patch.object(candidates.subprocess, "run") as run,
        ):
            candidates.discover(
                SimpleNamespace(tag=TAG, output=output), REPOSITORY, "main"
            )
        run.assert_not_called()
        plan.assert_called_once_with(
            repo=self.root, main_ref="HEAD", tag=TAG, release_revision=REVISION
        )
        self.assertEqual(json.loads(output.read_text()), plan_value)
        self.assertEqual(
            json.loads((self.root / "engine.lock").read_text()), self.original
        )

    def test_unresolved_candidate_publishes_a_draft_with_the_conflict_report(self):
        self.proposal.write_text(
            json.dumps(
                proposal(
                    prepared=False,
                    resolution="human",
                    conflicts=["README.md"],
                )
            )
        )
        calls = []

        def api(path, *, data=None):
            calls.append((path, data))
            if path.endswith("per_page=100"):
                return []
            if "/git/ref/heads/" in path:
                raise candidates.GhError(1, ["gh"], b"HTTP 404")
            if path.endswith("/pulls"):
                return {"number": 7, "html_url": "https://example.invalid/pr/7"}
            if path.endswith("/dispatches"):
                return {"workflow_run_id": 789}
            raise AssertionError(path)

        run_publish(
            api,
            self.proposal,
            self.bundle,
            original=self.original,
            candidate=self.candidate,
        )
        pr = next(data for path, data in calls if path.endswith("/pulls"))
        self.assertTrue(pr["draft"])
        self.assertIn("Preparation failed", pr["body"])
        self.assertIn("`README.md`", pr["body"])
        self.assertNotIn("attacker", pr["body"])

    def test_existing_candidate_at_the_same_commit_is_not_rewritten(self):
        seen = []

        def api(path, *, data=None):
            if "/pulls?" in path:
                return [
                    {
                        "number": 7,
                        "state": "open",
                        "draft": True,
                        "merged_at": None,
                        "html_url": "https://example.invalid/pr/7",
                        "labels": [],
                    }
                ]
            if "/git/ref/heads/" in path:
                return {"object": {"sha": HEAD}}
            seen.append(path)
            return {}

        for _ in range(2):
            started = run_publish(
                api,
                self.proposal,
                self.bundle,
                original=self.original,
                candidate=self.candidate,
            )
            started["push_candidate"].assert_not_called()
        self.assertEqual(seen, [])

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
            raw = (json.dumps(self.candidate, indent=2) + "\n").encode()
            fixture["report"]["engine_lock_sha256"] = hashlib.sha256(raw).hexdigest()

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
                    candidates,
                    "lock_bytes_at",
                    side_effect=[json.dumps(self.original).encode(), raw],
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


class GhCommandTest(unittest.TestCase):
    def call(self, results):
        return (
            patch.object(candidates.subprocess, "run", side_effect=results),
            patch.object(candidates.time, "sleep"),
            patch.object(candidates.sys, "stderr", io.StringIO()),
        )

    def test_transient_failures_retry_with_backoff_then_succeed(self):
        runs = [
            subprocess.CompletedProcess([], 1, b"", b"HTTP 502 Bad Gateway"),
            subprocess.CompletedProcess([], 0, b'{"ok": true}', b""),
        ]
        run_patch, sleep_patch, stderr_patch = self.call(runs)
        with run_patch as run, sleep_patch as sleep, stderr_patch as stderr:
            self.assertEqual(candidates.api(f"repos/{REPOSITORY}"), {"ok": True})
        self.assertEqual(run.call_count, 2)
        self.assertEqual([call.args for call in sleep.call_args_list], [(2,)])
        self.assertIn("HTTP 502 Bad Gateway", stderr.getvalue())

    def test_transient_failures_stop_after_three_attempts_with_status_and_stderr(self):
        for message, status in (
            (b"HTTP 503 Service Unavailable", 503),
            (b"HTTP 429 Too Many Requests", 429),
            (b"API rate limit exceeded", None),
            (b"Get https://api.github.com: i/o timeout", None),
            (b"read: connection reset by peer", None),
            (b"unexpected EOF", None),
        ):
            run_patch, sleep_patch, stderr_patch = self.call(
                [subprocess.CompletedProcess([], 1, b"", message)] * 3
            )
            with (
                self.subTest(message=message),
                run_patch as run,
                sleep_patch as sleep,
                stderr_patch as stderr,
                self.assertRaises(candidates.GhError) as error,
            ):
                candidates.api(f"repos/{REPOSITORY}")
            self.assertEqual(run.call_count, 3)
            self.assertEqual([call.args for call in sleep.call_args_list], [(2,), (8,)])
            self.assertEqual(error.exception.status, status)
            self.assertIn(message.decode(), stderr.getvalue())

    def test_writes_are_not_retried(self):
        run_patch, sleep_patch, stderr_patch = self.call(
            [subprocess.CompletedProcess([], 1, b"", b"HTTP 502 Bad Gateway")]
        )
        with (
            run_patch as run,
            sleep_patch as sleep,
            stderr_patch,
            self.assertRaises(candidates.GhError),
        ):
            candidates.api(f"repos/{REPOSITORY}/merges", data={"sha": HEAD})
        self.assertEqual(run.call_count, 1)
        sleep.assert_not_called()

    def test_non_transient_failures_fail_immediately_with_parsed_status(self):
        run_patch, sleep_patch, stderr_patch = self.call(
            [subprocess.CompletedProcess([], 1, b"", b"HTTP 404 Not Found")]
        )
        with (
            run_patch as run,
            sleep_patch as sleep,
            stderr_patch as stderr,
            self.assertRaises(candidates.GhError) as error,
        ):
            candidates.api(f"repos/{REPOSITORY}/git/ref/heads/missing")
        self.assertEqual(error.exception.status, 404)
        self.assertEqual(run.call_count, 1)
        sleep.assert_not_called()
        self.assertIn("HTTP 404 Not Found", stderr.getvalue())

    def test_main_turns_gh_failures_into_a_nonzero_stop(self):
        with (
            patch.object(
                candidates.sys, "argv", ["controller", "observe", "--run-id", "123"]
            ),
            patch.dict(os.environ, {"GITHUB_REPOSITORY": REPOSITORY}),
            patch.object(
                candidates,
                "api",
                side_effect=candidates.GhError(1, ["gh"], b"HTTP 404"),
            ),
            patch.object(candidates.sys, "stderr", io.StringIO()),
        ):
            self.assertEqual(candidates.main(), 1)


def publish_fake(replies):
    """Route each mocked gh call by its path; anything unexpected fails the test."""

    def api(path, *, data=None, method="POST"):
        for matches, reply in replies:
            if matches(path, method):
                return reply(path, data) if callable(reply) else reply
        raise AssertionError(f"Unexpected gh call {method} {path}")

    return api


class LifecycleTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.original = lock()
        self.candidate = {**self.original, "revision": REVISION, "release_tag": TAG}
        self.proposal = self.root / "candidate.json"
        self.proposal.write_text(json.dumps(proposal()))
        self.bundle = self.root / "candidate.bundle"
        self.bundle.write_bytes(b"bundle")
        enabled = patch.dict(os.environ, {"NYACODE_ENGINE_UPGRADES_ENABLED": "true"})
        enabled.start()
        self.addCleanup(enabled.stop)

    def publish(self, api, *, output=None, commit=HEAD):
        self.proposal.write_text(json.dumps(proposal(commit=commit)))
        started = run_publish(
            api,
            self.proposal,
            self.bundle,
            original=self.original,
            candidate=self.candidate,
            commit=commit,
            output=output,
        )
        self.pushes = started["push_candidate"].call_args_list
        return started["api"].call_args_list

    def events(self, status_file):
        if not status_file.exists():
            return []
        return [json.loads(line) for line in status_file.read_text().splitlines()]

    def test_merged_candidate_is_a_recorded_noop(self):
        merged = {
            "number": 7,
            "state": "closed",
            "merged_at": "2026-10-01T00:00:00Z",
            "html_url": PR_URL,
            "labels": [],
        }
        status_file = self.root / "status.jsonl"
        with patch.dict(os.environ, {"NYACODE_STATUS_FILE": str(status_file)}):
            calls = self.publish(
                publish_fake([(lambda p, m: "/pulls?" in p, [merged])])
            )
        self.assertEqual(len(calls), 1)
        self.assertEqual(
            [event["kind"] for event in self.events(status_file)], ["merged"]
        )

    def test_rejected_closed_candidate_is_a_noop(self):
        rejected = {
            "number": 7,
            "state": "closed",
            "merged_at": None,
            "html_url": PR_URL,
            "labels": [{"name": "engine:reject"}],
        }
        calls = self.publish(publish_fake([(lambda p, m: "/pulls?" in p, [rejected])]))
        self.assertEqual(len(calls), 1)

    def test_closed_unlabelled_candidate_gets_a_new_pull_request_on_a_rebuilt_branch(
        self,
    ):
        closed = {
            "number": 7,
            "state": "closed",
            "merged_at": None,
            "html_url": PR_URL,
            "labels": [],
        }
        output = self.root / "github-output"
        calls = self.publish(
            publish_fake(
                [
                    (lambda p, m: "/pulls?" in p, [closed]),
                    (
                        lambda p, m: p.endswith("/git/ref/heads/engine/rust-v1.2.3"),
                        {"object": {"sha": HEAD}},
                    ),
                    (
                        lambda p, m: p.endswith("/pulls"),
                        {"number": 8, "html_url": "https://example.invalid/pr/8"},
                    ),
                    (lambda p, m: p.endswith("/dispatches"), {"workflow_run_id": 789}),
                ]
            ),
            output=output,
        )
        self.assertEqual(
            self.pushes, [call(REPOSITORY, HEAD, "engine/rust-v1.2.3", force=True)]
        )
        pr = next(c for c in calls if c.args[0].endswith("/pulls"))
        self.assertFalse(pr.kwargs["data"]["draft"])
        dispatch = next(c for c in calls if c.args[0].endswith("/dispatches"))
        self.assertEqual(dispatch.kwargs["data"]["inputs"]["pull_request"], "8")
        self.assertEqual(output.read_text(), "qualification_run=789\n")

    def test_stale_open_candidate_is_force_rebuilt_and_redispatched(self):
        open_pr = {
            "number": 7,
            "state": "open",
            "draft": False,
            "merged_at": None,
            "html_url": PR_URL,
            "labels": [],
        }
        calls = self.publish(
            publish_fake(
                [
                    (lambda p, m: "/pulls?" in p, [open_pr]),
                    (
                        lambda p, m: p.endswith("/git/ref/heads/engine/rust-v1.2.3"),
                        {"object": {"sha": HEAD}},
                    ),
                    (
                        lambda p, m: p.endswith(f"/git/commits/{HEAD}"),
                        {"parents": [{"sha": OLD}]},
                    ),
                    (lambda p, m: p.endswith("/dispatches"), {"workflow_run_id": 789}),
                ]
            ),
            commit=NEW,
        )
        self.assertEqual(
            self.pushes, [call(REPOSITORY, NEW, "engine/rust-v1.2.3", force=True)]
        )
        self.assertFalse([c for c in calls if c.args[0].endswith("/pulls")])
        dispatch = next(c for c in calls if c.args[0].endswith("/dispatches"))
        self.assertEqual(dispatch.kwargs["data"]["inputs"]["pull_request"], "7")

    def test_publishing_a_newer_tag_closes_older_candidates_with_a_comment(self):
        def pr(number, ref, repo=REPOSITORY):
            return {
                "number": number,
                "html_url": f"https://example.invalid/pr/{number}",
                "base": {"ref": "main"},
                "head": {"ref": ref, "sha": HEAD, "repo": {"full_name": repo}},
            }

        open_prs = [
            pr(3, "engine/rust-v1.2.2"),
            pr(4, "engine/rust-v1.2.4"),
            pr(5, "feature/x"),
            pr(6, "engine/rust-v1.2.0", repo="attacker/fork"),
        ]
        calls = []

        def api(path, *, data=None, method="POST"):
            calls.append((path, data, method))
            return {}

        with (
            patch.object(candidates, "pages", return_value=open_prs),
            patch.object(candidates, "api", side_effect=api),
        ):
            candidates.supersede(REPOSITORY, "main", TAG, PR_URL)
        self.assertEqual(
            calls,
            [
                (
                    f"repos/{REPOSITORY}/issues/3/comments",
                    {"body": f"Superseded by {PR_URL}"},
                    "POST",
                ),
                (f"repos/{REPOSITORY}/pulls/3", {"state": "closed"}, "PATCH"),
            ],
        )

    def test_sweep_promotes_a_completed_successful_candidate_without_waiting(self):
        pr = {
            "number": 7,
            "html_url": PR_URL,
            "base": {"ref": "main"},
            "head": {
                "ref": f"engine/{TAG}",
                "sha": HEAD,
                "repo": {"full_name": REPOSITORY},
            },
        }
        run = {
            "id": 123,
            "event": "workflow_dispatch",
            "head_branch": f"engine/{TAG}",
            "status": "completed",
            "conclusion": "success",
        }
        with (
            patch.object(candidates, "pages", return_value=[pr]),
            patch.object(
                candidates, "api", return_value={"workflow_runs": [run]}
            ) as api,
            patch.object(candidates, "observe", return_value=True) as observe,
            patch.object(candidates, "promote") as promote,
        ):
            candidates.sweep(SimpleNamespace(), REPOSITORY, "main")
        self.assertEqual(
            api.call_args.args[0].split("?")[0],
            f"repos/{REPOSITORY}/actions/workflows/{candidates.WORKFLOW}/runs",
        )
        self.assertEqual(observe.call_args.args[0].wait_seconds, 0)
        self.assertEqual(promote.call_args.args[0].run_id, 123)

    def test_sweep_ignores_in_progress_candidates(self):
        pr = {
            "number": 7,
            "html_url": PR_URL,
            "base": {"ref": "main"},
            "head": {
                "ref": f"engine/{TAG}",
                "sha": HEAD,
                "repo": {"full_name": REPOSITORY},
            },
        }
        run = {
            "id": 123,
            "event": "workflow_dispatch",
            "head_branch": f"engine/{TAG}",
            "status": "in_progress",
            "conclusion": None,
        }
        with (
            patch.object(candidates, "pages", return_value=[pr]),
            patch.object(candidates, "api", return_value={"workflow_runs": [run]}),
            patch.object(candidates, "observe") as observe,
            patch.object(candidates, "promote") as promote,
        ):
            candidates.sweep(SimpleNamespace(), REPOSITORY, "main")
        observe.assert_not_called()
        promote.assert_not_called()

    def test_disabled_publish_refuses_before_any_github_request_and_records_it(self):
        status_file = self.root / "status.jsonl"
        with (
            patch.dict(
                os.environ,
                {
                    "NYACODE_ENGINE_UPGRADES_ENABLED": "",
                    "NYACODE_STATUS_FILE": str(status_file),
                },
            ),
            patch.object(candidates, "api") as api,
            patch.object(candidates, "current_base") as base,
        ):
            candidates.publish(
                SimpleNamespace(proposal=self.proposal), REPOSITORY, "main"
            )
        api.assert_not_called()
        base.assert_not_called()
        self.assertEqual(
            [event["kind"] for event in self.events(status_file)], ["disabled"]
        )

    def promote_with_lock(self, raw, digest):
        fixture = qualification()
        fixture["report"]["engine_lock_sha256"] = digest

        def api(path, *, data=None, method="POST"):
            if path.endswith("/runs/123"):
                return fixture["run"]
            if "/workflows/" in path:
                return {"id": 456}
            if path == "graphql":
                return {
                    "data": {
                        "repository": {"ref": {"branchProtectionRule": protection()}}
                    }
                }
            if path.endswith("/pulls/7"):
                return {**fixture["pr"], "mergeable_state": "clean"}
            return {"parents": [{"sha": BASE}]}

        with (
            patch.object(candidates, "api", side_effect=api),
            patch.object(candidates, "report_for", return_value=fixture["report"]),
            patch.object(candidates, "current_base", side_effect=[BASE, BASE]),
            patch.object(candidates, "pages", return_value=fixture["changed_files"]),
            patch.object(candidates, "run_jobs", return_value=fixture["jobs"]),
            patch.object(
                candidates,
                "lock_bytes_at",
                side_effect=[json.dumps(self.original).encode(), raw],
            ),
            patch.object(candidates, "stable_release", return_value=REVISION),
            patch.object(candidates, "command", return_value={"merged": True}) as merge,
        ):
            candidates.promote(SimpleNamespace(run_id=123), REPOSITORY, "main")
        return merge

    def test_promotion_digest_is_taken_over_the_raw_engine_lock_bytes(self):
        raw = json.dumps(self.candidate).encode()
        self.promote_with_lock(
            raw, hashlib.sha256(raw).hexdigest()
        ).assert_called_once()
        pretty = (json.dumps(self.candidate, indent=2) + "\n").encode()
        with self.assertRaisesRegex(ValueError, "differs from the candidate pin"):
            self.promote_with_lock(raw, hashlib.sha256(pretty).hexdigest())


class WorkflowScheduleTest(unittest.TestCase):
    def test_three_hourly_sweep_and_kill_switch_are_wired_through_the_workflow(self):
        workflow = (
            candidates.ROOT / ".github/workflows/engine-upgrades.yml"
        ).read_text()
        self.assertIn('cron: "17 */3 * * *"', workflow)
        sweep = workflow.split("\n  sweep:\n", 1)[1].split("\n  status:\n", 1)[0]
        self.assertIn(
            "contents: write\n      pull-requests: write\n      actions: read", sweep
        )
        self.assertIn("python3 scripts/engine_candidates.py sweep", sweep)
        publisher = workflow.split("\n  publish-candidate:\n", 1)[1].split(
            "\n  observe:\n", 1
        )[0]
        self.assertIn(
            "NYACODE_ENGINE_UPGRADES_ENABLED: ${{ vars.NYACODE_ENGINE_UPGRADES_ENABLED }}",
            publisher,
        )
        status = workflow.split("\n  status:\n", 1)[1]
        self.assertIn("contents: read\n      issues: write", status)


class CandidateAllowlistTest(unittest.TestCase):
    def test_only_the_candidate_allowlist_may_change(self):
        for path in (
            "engine.lock",
            "inventory/downstream.json",
            "integration/codex/Cargo.lock",
            "integration/codex/assembly.json",
            "integration/codex/ownership.json",
            "integration/codex/patches/documentation.patch",
            "integration/codex/generated/codex-rs/x.rs",
            "integration/codex/compatibility/rpc.json",
        ):
            with self.subTest(path=path):
                self.assertEqual(
                    candidates.select_mode([path], f"engine/{TAG}"), "full"
                )
        for path in (
            ".github/workflows/engine-qualification.yml",
            "scripts/engine.py",
            "VERSION",
            "engine.lock.bak",
            "integration/codex/adapter/src/lib.rs",
            "integration/codex/overlays/note.txt",
            "integration/codex/patches-extra/x.patch",
            "integration/codex/patches/../engine.lock",
            "/etc/passwd",
        ):
            with self.subTest(path=path), self.assertRaises(ValueError):
                candidates.select_mode([path], f"engine/{TAG}")
        with self.assertRaises(ValueError):
            candidates.select_mode([], f"engine/{TAG}")


class ApprovalTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.original = lock()

    def promote(self, reviews=(), permissions=None, classification="assisted"):
        fixture = qualification()
        fixture["report"]["classification"] = classification
        fixture["report"]["classification_reasons"] = ["conflicts in 1 files"]
        fixture["pr"]["html_url"] = PR_URL
        raw = json.dumps({**self.original, "revision": REVISION, "release_tag": TAG})
        fixture["report"]["engine_lock_sha256"] = hashlib.sha256(
            raw.encode()
        ).hexdigest()
        permissions = permissions or {}

        def api(path, *, data=None, method="POST"):
            if path.endswith("/runs/123"):
                return fixture["run"]
            if "/workflows/" in path:
                return {"id": 456}
            if path == "graphql":
                return {
                    "data": {
                        "repository": {"ref": {"branchProtectionRule": protection()}}
                    }
                }
            if path.endswith("/pulls/7"):
                return {**fixture["pr"], "mergeable_state": "clean"}
            if "/collaborators/" in path:
                login = path.split("/collaborators/")[1].split("/")[0]
                return {"permission": permissions.get(login, "read")}
            if path.endswith("/labels"):
                return []
            return {"parents": [{"sha": BASE}]}

        def pages(path, field=None):
            return list(reviews) if "/reviews" in path else fixture["changed_files"]

        status = self.root / "status.jsonl"
        with (
            patch.dict(os.environ, {"NYACODE_STATUS_FILE": str(status)}),
            patch.object(candidates, "api", side_effect=api) as gh,
            patch.object(candidates, "report_for", return_value=fixture["report"]),
            patch.object(candidates, "current_base", side_effect=[BASE, BASE]),
            patch.object(candidates, "pages", side_effect=pages),
            patch.object(candidates, "run_jobs", return_value=fixture["jobs"]),
            patch.object(
                candidates,
                "lock_bytes_at",
                side_effect=[json.dumps(self.original).encode(), raw.encode()],
            ),
            patch.object(candidates, "stable_release", return_value=REVISION),
            patch.object(candidates, "command", return_value={"merged": True}) as merge,
        ):
            candidates.promote(SimpleNamespace(run_id=123), REPOSITORY, "main")
        events = (
            [json.loads(line) for line in status.read_text().splitlines()]
            if status.exists()
            else []
        )
        return merge, gh.call_args_list, events

    def review(self, review_id, login, state, commit=HEAD):
        return {
            "id": review_id,
            "user": {"login": login},
            "state": state,
            "commit_id": commit,
        }

    def test_assisted_candidate_without_approval_waits_and_is_labelled(self):
        merge, calls, events = self.promote()
        merge.assert_not_called()
        labels = [c for c in calls if c.args[0].endswith("/labels")]
        self.assertEqual(
            labels[0].kwargs["data"], {"labels": ["engine:needs-approval"]}
        )
        waiting = [event for event in events if event["kind"] == "awaiting-approval"]
        self.assertEqual(len(waiting), 1)
        self.assertIn(f"awaiting approval for {PR_URL}", waiting[0]["message"])
        self.assertIn("assisted: conflicts in 1 files", waiting[0]["message"])

    def test_approval_of_an_older_commit_does_not_count(self):
        merge, _, _ = self.promote(
            [self.review(1, "maint", "APPROVED", commit=OLD)],
            {"maint": "write"},
        )
        merge.assert_not_called()

    def test_approval_by_a_reader_does_not_count(self):
        merge, _, _ = self.promote(
            [self.review(1, "reader", "APPROVED")], {"reader": "read"}
        )
        merge.assert_not_called()

    def test_writer_approval_of_the_exact_head_merges_that_head(self):
        merge, _, _ = self.promote(
            [self.review(1, "maint", "APPROVED")], {"maint": "maintain"}
        )
        merge.assert_called_once_with(
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

    def test_change_request_from_a_writer_after_approval_blocks_the_merge(self):
        merge, _, _ = self.promote(
            [
                self.review(1, "maint", "APPROVED"),
                self.review(2, "lead", "CHANGES_REQUESTED"),
            ],
            {"maint": "write", "lead": "admin"},
        )
        merge.assert_not_called()

    def test_change_request_from_a_reader_does_not_block_a_writer_approval(self):
        merge, _, _ = self.promote(
            [
                self.review(1, "maint", "APPROVED"),
                self.review(2, "reader", "CHANGES_REQUESTED"),
            ],
            {"maint": "write", "reader": "read"},
        )
        merge.assert_called_once()

    def test_a_dismissed_approval_does_not_count(self):
        merge, _, _ = self.promote(
            [
                self.review(1, "maint", "APPROVED"),
                self.review(2, "maint", "DISMISSED"),
            ],
            {"maint": "write"},
        )
        merge.assert_not_called()

    def test_mechanical_candidates_merge_without_approval(self):
        merge, _, events = self.promote(classification="mechanical")
        merge.assert_called_once()
        self.assertEqual(
            [event["kind"] for event in events if event["kind"] == "promoted"],
            ["promoted"],
        )

    def test_missing_classification_fails_closed(self):
        fixture = qualification()
        del fixture["report"]["classification"]
        with self.assertRaisesRegex(ValueError, "classification"):
            candidates.validate_promotion(**fixture)


class QualifiedMergeTest(unittest.TestCase):
    MERGE = "f" * 40

    def check(
        self,
        *,
        pulls=None,
        merge=None,
        head_tree="tree-a",
        report=None,
        conclusion="success",
        parents=None,
        api_error=None,
    ):
        fixture = qualification()
        report = {**fixture["report"], **(report or {})}
        pr = fixture["pr"] | {
            "merged_at": "2026-10-26T00:00:00Z",
            "merge_commit_sha": self.MERGE,
            "base": {"ref": "main", "sha": BASE},
        }
        run = {**fixture["run"], "conclusion": conclusion}
        if pulls is None:
            pulls = [pr]
        merge_commit = merge or {
            "parents": parents or [{"sha": BASE}],
            "tree": {"sha": "tree-a"},
        }

        def api(path, *, data=None, method="POST"):
            if api_error is not None:
                raise api_error
            if path == f"repos/{REPOSITORY}":
                return {"default_branch": "main"}
            if path.endswith(f"/commits/{self.MERGE}/pulls"):
                return pulls
            if path.endswith(f"/git/commits/{self.MERGE}"):
                return merge_commit
            if path.endswith(f"/git/commits/{HEAD}"):
                return {"tree": {"sha": head_tree}}
            if "/workflows/" in path and "/runs?" not in path:
                return {"id": 456}
            if "/runs?" in path:
                return {"workflow_runs": [run]}
            raise AssertionError(path)

        with (
            patch.object(candidates, "api", side_effect=api),
            patch.object(candidates, "report_for", return_value=report),
            patch.object(candidates, "run_jobs", return_value=fixture["jobs"]),
        ):
            return candidates.qualified_merge(REPOSITORY, self.MERGE)

    def test_exact_qualified_squash_is_selected_for_controls(self):
        self.assertTrue(self.check())

    def test_each_doubt_fails_closed(self):
        cases = {
            "tree differs": {"head_tree": "tree-b"},
            "two parents": {"parents": [{"sha": BASE}, {"sha": OLD}]},
            "wrong base in report": {"report": {"base_sha": REVISION}},
            "wrong PR in report": {"report": {"pull_request": 8}},
            "failed run": {"conclusion": "failure"},
            "unmerged PR": {"pulls": [{"number": 7, "merged_at": None}]},
            "two matching PRs": {
                "pulls": [qualification()["pr"], qualification()["pr"]]
            },
            "API error": {"api_error": candidates.GhError(1, ["gh"], b"HTTP 500")},
        }
        for name, kwargs in cases.items():
            with self.subTest(name=name):
                self.assertFalse(self.check(**kwargs))

    def test_non_sha_inputs_are_never_qualified(self):
        self.assertFalse(candidates.qualified_merge(REPOSITORY, "main"))


class ReviewWorkflowTest(unittest.TestCase):
    PATH = candidates.ROOT / ".github/workflows/engine-review.yml"

    def test_approval_wiring_is_text_checked_without_parsing(self):
        text = self.PATH.read_text()
        self.assertIn("pull_request_review:", text)
        self.assertIn("types: [submitted]", text)
        self.assertIn("github.event.review.state == 'approved'", text)
        self.assertIn(
            "startsWith(github.event.pull_request.head.ref, 'engine/rust-v')", text
        )
        self.assertIn(
            "github.event.pull_request.head.repo.full_name == github.repository", text
        )
        self.assertIn("actions: write", text)
        self.assertIn("gh workflow run engine-upgrades.yml", text)
        self.assertNotIn("actions/checkout", text)
        self.assertNotIn("head.sha", text)

    def test_yaml_grants_only_actions_write_and_checks_no_pr_code(self):
        try:
            import yaml
        except ImportError:
            self.skipTest("PyYAML is not installed")
        workflow = yaml.safe_load(self.PATH.read_text())
        trigger = workflow.get(True, workflow.get("on"))
        self.assertEqual(trigger["pull_request_review"]["types"], ["submitted"])
        self.assertEqual(workflow["permissions"], {})
        job = workflow["jobs"]["sweep"]
        self.assertEqual(job["permissions"], {"actions": "write"})
        self.assertFalse(
            any("checkout" in str(step.get("uses", "")) for step in job["steps"])
        )


class CandidateWorkflowTest(unittest.TestCase):
    def read(self, name):
        return (candidates.ROOT / ".github/workflows" / name).read_text()

    def test_construction_is_read_only_and_publication_consumes_only_the_bundle(self):
        text = self.read("engine-upgrades.yml")
        construct = text.split("\n  construct:\n", 1)[1].split("\n  resolve:\n", 1)[0]
        self.assertIn("contents: read", construct)
        self.assertNotIn("contents: write", construct)
        self.assertIn("scripts/engine_sync.py rebase --plan", construct)
        build = text.split("\n  build:\n", 1)[1].split("\n  publish-candidate:\n", 1)[0]
        self.assertIn("contents: read", build)
        self.assertIn('args=(finish --plan "$RUNNER_TEMP/plan/plan.json"', build)
        self.assertIn("--allow-unresolved", build)
        publisher = text.split("\n  publish-candidate:\n", 1)[1].split(
            "\n  observe:\n", 1
        )[0]
        self.assertIn("contents: write", publisher)
        self.assertIn("scripts/engine_candidates.py publish --proposal", publisher)
        self.assertIn("--bundle", publisher)
        self.assertIn("needs: [discover, build]", publisher)

    def test_qualification_classifies_and_gates_the_verdict(self):
        text = self.read("engine-qualification.yml")
        classify = text.split("\n  classify:\n", 1)[1].split("\n  verdict:\n", 1)[0]
        self.assertIn("scripts/engine_sync.py classify", classify)
        # Promotion matches qualification runs by job display name.
        self.assertIn("    name: Classify candidate\n", classify)
        self.assertIn("Classify candidate", candidates.REQUIRED_JOBS)
        self.assertIn("contents: read", classify)
        self.assertNotIn("contents: write", classify)
        verdict = text.split("\n  verdict:\n", 1)[1]
        self.assertIn("classify", verdict.split("steps:", 1)[0])
        self.assertIn("classification", verdict)

    def test_acceptance_selects_controls_for_qualified_merges(self):
        text = self.read("engine-acceptance.yml")
        select = text.split("\n  select:\n", 1)[1].split("\n  controls:\n", 1)[0]
        self.assertIn("--merge-sha", select)
        self.assertIn(
            "MERGE_SHA: ${{ github.event_name == 'push' && github.sha || '' }}", select
        )


if __name__ == "__main__":
    unittest.main()
