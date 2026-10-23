import contextlib
import io
import subprocess
import sys
import unittest
from unittest.mock import Mock, patch

import run_v8_bazel_build as build

FETCH_FAILURE = """Error in fail: error running 'git fetch origin refs/heads/*' while working with @dragonbox:
fatal: unable to access 'https://example.test/dragonbox.git/': The requested URL returned error: 503
ERROR: Analysis of target '//third_party/v8:release_pair' failed; build aborted: Analysis failed
"""


class V8BuildRetryTest(unittest.TestCase):
    def run_build(self, outcomes):
        attempt = Mock(side_effect=outcomes)
        pause = Mock()
        with contextlib.redirect_stdout(io.StringIO()):
            status = build.run_build(
                ["bazel", "build", "//third_party/v8:pair"], attempt, pause
            )
        return status, attempt, pause

    def test_recovers_from_transient_repository_fetch_failures(self):
        for code in (500, 502, 503, 504):
            with self.subTest(code=code):
                status, attempt, pause = self.run_build(
                    [(1, FETCH_FAILURE.replace("503", str(code))), (0, "build passed")]
                )
                self.assertEqual(status, 0)
                self.assertEqual(attempt.call_count, 2)
                pause.assert_called_once_with(10)

    def test_exhausted_retries_preserve_failure(self):
        status, attempt, pause = self.run_build([(1, FETCH_FAILURE)] * 3)
        self.assertEqual(status, 1)
        self.assertEqual(attempt.call_count, 3)
        self.assertEqual([call.args for call in pause.call_args_list], [(10,), (20,)])

    def test_compile_authentication_certificate_and_other_failures_are_not_retried(
        self,
    ):
        failures = [
            (1, "ERROR: compilation failed: undefined symbol"),
            (1, "ERROR: Analysis failed: missing target"),
            (1, FETCH_FAILURE.replace("503", "403")),
            (1, FETCH_FAILURE.replace("503", "404")),
            (
                1,
                FETCH_FAILURE.replace(
                    "The requested URL returned error: 503", "SSL certificate problem"
                ),
            ),
            (1, FETCH_FAILURE.replace("Analysis failed", "Build failed")),
            (1, FETCH_FAILURE.replace("git fetch", "git checkout")),
            (130, FETCH_FAILURE),
            (-15, FETCH_FAILURE),
        ]
        for failure in failures:
            with self.subTest(failure=failure):
                status, attempt, pause = self.run_build([failure])
                self.assertEqual(status, failure[0])
                self.assertEqual(attempt.call_count, 1)
                pause.assert_not_called()

    def test_success_does_not_retry_even_if_output_mentions_a_fetch_error(self):
        status, attempt, pause = self.run_build([(0, FETCH_FAILURE)])
        self.assertEqual(status, 0)
        self.assertEqual(attempt.call_count, 1)
        pause.assert_not_called()

    def test_interrupt_does_not_retry(self):
        attempt = Mock(side_effect=KeyboardInterrupt)
        pause = Mock()
        with self.assertRaises(KeyboardInterrupt):
            build.run_build(["bazel", "build"], attempt, pause)
        self.assertEqual(attempt.call_count, 1)
        pause.assert_not_called()

    def test_streams_all_output_while_retaining_a_bounded_diagnostic_tail(self):
        size = build.TAIL_BYTES + 100
        output = io.BytesIO()
        status, tail = build.run_once(
            [
                sys.executable,
                "-c",
                f"import sys; sys.stdout.write('x' * {size} + 'done'); sys.exit(7)",
            ],
            output,
        )
        self.assertEqual(status, 7)
        self.assertEqual(len(output.getvalue()), size + 4)
        self.assertEqual(len(tail), build.TAIL_BYTES)
        self.assertTrue(tail.endswith("done"))

    def test_interrupted_output_terminates_and_reaps_the_child(self):
        class InterruptedOutput(io.BytesIO):
            def write(self, data):
                raise KeyboardInterrupt

        children = []
        popen = subprocess.Popen

        def start(*args, **kwargs):
            child = popen(*args, **kwargs)
            children.append(child)
            return child

        try:
            with (
                patch.object(build.subprocess, "Popen", side_effect=start),
                self.assertRaises(KeyboardInterrupt),
            ):
                build.run_once(
                    [
                        sys.executable,
                        "-c",
                        "import time; print('ready', flush=True); time.sleep(60)",
                    ],
                    InterruptedOutput(),
                )
            self.assertIsNotNone(children[0].poll())
            self.assertNotEqual(children[0].returncode, 0)
        finally:
            for child in children:
                if child.poll() is None:
                    child.kill()
                    child.wait()

    def test_rejects_commands_that_may_execute_user_code_or_tests(self):
        for command in ("run", "test", "clean"):
            with self.subTest(command=command):
                with (
                    patch.object(sys, "argv", ["retry", command, "//target"]),
                    patch.object(build, "run_build") as run,
                    self.assertRaisesRegex(
                        SystemExit, "requires a Bazel build command"
                    ),
                ):
                    build.main()
                run.assert_not_called()


if __name__ == "__main__":
    unittest.main()
