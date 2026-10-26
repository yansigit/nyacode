"""Upstream sync events are recorded as JSON lines and kept in one status issue."""

import importlib.util
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location(
    "engine_status", Path(__file__).resolve().parents[1] / "engine_status.py"
)
status = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(status)
REPOSITORY = "owner/product"
ISSUE_URL = "https://example.invalid/issues/5"


class RecordTest(unittest.TestCase):
    def test_record_appends_json_lines_only_when_a_status_file_is_configured(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "status" / "job.jsonl"
            with patch.dict(os.environ, {"NYACODE_STATUS_FILE": str(path)}):
                status.record(
                    "published", "Published rust-v1.2.3", "https://example.invalid/pr/7"
                )
                status.record("skipped", "Nothing to do")
            events = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual(
                [event["kind"] for event in events], ["published", "skipped"]
            )
            self.assertEqual(events[0]["url"], "https://example.invalid/pr/7")
            self.assertNotIn("url", events[1])
            with patch.dict(os.environ, {}):
                os.environ.pop("NYACODE_STATUS_FILE", None)
                self.assertIsNone(status.record("skipped", "No file configured"))
            self.assertEqual(len(path.read_text().splitlines()), 2)


class PublishTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        (self.root / "engine.lock").write_text(
            json.dumps({"release_tag": "rust-v0.164.0", "revision": "a" * 40})
        )
        self.status_dir = self.root / "status"
        self.status_dir.mkdir()

    def run_publish(self, issues):
        calls = []

        def fake_gh(path, *, data=None, method="GET"):
            calls.append((path, data, method))
            if path.startswith("repos/owner/product/issues?"):
                return issues
            return {"html_url": ISSUE_URL}

        with (
            patch.object(status, "ROOT", self.root),
            patch.object(status, "gh", side_effect=fake_gh),
            patch("builtins.print"),
        ):
            status.publish(REPOSITORY, self.status_dir)
        return calls

    def test_publish_upserts_one_issue_and_keeps_the_newest_thirty_events(self):
        previous = [
            {
                "time": f"2026-10-01T00:00:{index:02d}Z",
                "kind": "old",
                "message": f"event {index}",
            }
            for index in range(30)
        ]
        body = (
            "Header\n\n"
            + status.START
            + "\n"
            + "\n".join(status.render_event(event) for event in previous)
            + "\n"
            + status.END
        )
        (self.status_dir / "job.jsonl").write_text(
            "\n".join(
                json.dumps({"time": time, "kind": "new", "message": message})
                for time, message in (
                    ("2026-10-26T01:00:00Z", "first new"),
                    ("2026-10-26T02:00:00Z", "second new"),
                )
            )
            + "\n"
        )
        issue = {
            "number": 5,
            "title": status.TITLE,
            "body": body,
            "html_url": ISSUE_URL,
        }
        calls = self.run_publish([issue])
        path, data, method = calls[-1]
        self.assertEqual((path, method), ("repos/owner/product/issues/5", "PATCH"))
        new_body = data["body"]
        events = status.previous_events(new_body)
        self.assertEqual(len(events), 30)
        self.assertEqual(
            [event["message"] for event in events[:2]], ["second new", "first new"]
        )
        self.assertNotIn("event 0", new_body)
        self.assertNotIn("event 1\n", new_body)
        self.assertIn("event 29", new_body)
        self.assertEqual(new_body.count(status.START), 1)
        self.assertIn("Current pin: `rust-v0.164.0`", new_body)

    def test_publish_creates_the_labelled_issue_when_none_is_open(self):
        calls = self.run_publish([])
        path, data, method = calls[-1]
        self.assertEqual((path, method), ("repos/owner/product/issues", "POST"))
        self.assertEqual(data["title"], status.TITLE)
        self.assertEqual(data["labels"], [status.LABEL])

    def test_publish_ignores_pull_requests_and_refuses_duplicate_status_issues(self):
        same = {"number": 5, "title": status.TITLE, "body": "", "html_url": ISSUE_URL}
        pull = {**same, "number": 6, "pull_request": {}}
        self.assertEqual(self.run_publish([pull])[-1][0], "repos/owner/product/issues")
        with self.assertRaisesRegex(ValueError, "at most one"):
            self.run_publish([same, {**same, "number": 9}])


class FailureTest(unittest.TestCase):
    def test_failed_and_cancelled_jobs_get_failure_events_with_the_run_url(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "status.jsonl"
            results = {
                "observe": {"result": "failure"},
                "promote": {"result": "cancelled"},
                "sweep": {"result": "success"},
                "discover": {"result": "skipped"},
            }
            with patch.dict(
                os.environ,
                {
                    "NYACODE_STATUS_FILE": str(path),
                    "RESULTS": json.dumps(results),
                    "RUN_URL": "https://example.invalid/run/1",
                },
            ):
                status.failures()
            events = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual(
                [event["message"] for event in events],
                ["Job observe ended with failure", "Job promote ended with cancelled"],
            )
            self.assertEqual(
                {event["url"] for event in events}, {"https://example.invalid/run/1"}
            )


if __name__ == "__main__":
    unittest.main()
