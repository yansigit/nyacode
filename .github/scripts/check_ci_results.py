#!/usr/bin/env python3

"""Fail a terminal CI job unless every serialized dependency succeeded.

Parent workflows pass GitHub's `toJSON(needs)` object through the NEEDS
environment variable. Skipped jobs fail unless the caller explicitly allows
them; failed and cancelled dependencies always fail.
"""

import json
import os


def main() -> None:
    # Keep result policy in one script so blocking-ci and postmerge-ci cannot
    # drift in how they interpret dependency conclusions.
    needs = json.loads(os.environ["NEEDS"])
    allowed_skipped = set(os.environ.get("ALLOWED_SKIPPED_JOBS", "").split(","))
    failures = sorted(
        (name, dependency["result"])
        for name, dependency in needs.items()
        if dependency["result"] != "success"
        and not (dependency["result"] == "skipped" and name in allowed_skipped)
    )

    if failures:
        print("CI dependencies did not succeed:")
        for name, result in failures:
            print(f"{name}: {result}")
        raise SystemExit(1)

    print("All required CI dependencies succeeded.")


if __name__ == "__main__":
    main()
