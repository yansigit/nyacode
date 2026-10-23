import contextlib
import io
import json
import os
import unittest
from unittest.mock import patch

import check_ci_results


class RequiredCIResultsTests(unittest.TestCase):
    def check(self, jobs, allowed=""):
        env = {
            "NEEDS": json.dumps(
                {name: {"result": result} for name, result in jobs.items()}
            ),
            "ALLOWED_SKIPPED_JOBS": allowed,
        }
        with patch.dict(os.environ, env), contextlib.redirect_stdout(io.StringIO()):
            check_ci_results.main()

    def test_success_and_explicitly_skipped_upstream_jobs_pass(self):
        self.check({"cargo-deny": "success", "bazel": "skipped"}, "bazel,rust-ci,sdk")

    def test_unlisted_skipped_security_job_fails(self):
        with self.assertRaises(SystemExit):
            self.check({"cargo-deny": "skipped"}, "bazel,rust-ci,sdk")

    def test_failed_and_cancelled_allowed_jobs_still_fail(self):
        for result in ["failure", "cancelled"]:
            with self.subTest(result=result), self.assertRaises(SystemExit):
                self.check({"bazel": result}, "bazel")

    def test_skip_requires_explicit_caller_allowlist(self):
        with self.assertRaises(SystemExit):
            self.check({"bazel": "skipped"})


if __name__ == "__main__":
    unittest.main()
