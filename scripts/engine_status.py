#!/usr/bin/env python3
"""Record upstream engine sync events and publish them as one status issue."""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TITLE = "Upstream sync status"
LABEL = "engine:status"
LIMIT = 30
START = "<!-- events:start -->"
END = "<!-- events:end -->"
EVENT = re.compile(r"<!-- (\{.*\}) -->$")


def now() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def record(kind: str, message: str, url: str | None = None) -> None:
    path = os.environ.get("NYACODE_STATUS_FILE")
    if not path:
        return
    event = {"time": now(), "kind": kind, "message": message}
    if url:
        event["url"] = url
    Path(path).parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a") as stream:
        stream.write(json.dumps(event) + "\n")


def render_event(event: dict) -> str:
    link = f" ([link]({event['url']}))" if event.get("url") else ""
    # Escaped dashes keep the embedded JSON from closing the HTML comment early.
    data = json.dumps(event, sort_keys=True).replace("--", "\\u002d\\u002d")
    return f"- `{event['time']}` **{event['kind']}**: {event['message']}{link} <!-- {data} -->"


def previous_events(body: str) -> list[dict]:
    if START not in body or END not in body:
        return []
    section = body.split(START, 1)[1].split(END, 1)[0]
    events = []
    for text in section.splitlines():
        match = EVENT.search(text.strip())
        if match:
            try:
                events.append(json.loads(match.group(1)))
            except json.JSONDecodeError:
                continue
    return events


def read_events(directory: Path) -> list[dict]:
    events = []
    for path in sorted(directory.glob("*.jsonl")):
        events += [json.loads(text) for text in path.read_text().splitlines() if text]
    return events


def gh(path: str, *, data: dict | None = None, method: str = "GET"):
    args = ["gh", "api", path]
    if data is not None:
        args += ["--method", method, "--input", "-"]
    result = subprocess.run(
        args,
        cwd=ROOT,
        input=json.dumps(data).encode() if data is not None else None,
        capture_output=True,
        check=False,
    )
    if result.returncode != 0:
        sys.stderr.write(result.stderr.decode(errors="replace"))
        raise subprocess.CalledProcessError(result.returncode, args)
    return json.loads(result.stdout)


def render(pin: dict, events: list[dict]) -> str:
    return "\n".join(
        [
            f"Updated `{now()}`.",
            "",
            f"Current pin: `{pin['release_tag']}` at `{pin['revision']}`.",
            "",
            "## Recent events",
            "",
            START,
            *(render_event(event) for event in events),
            END,
            "",
        ]
    )


def publish(repository: str, status_dir: Path) -> None:
    pin = json.loads((ROOT / "engine.lock").read_text())
    issues = [
        issue
        for issue in gh(
            f"repos/{repository}/issues?state=open&labels={LABEL}&per_page=100"
        )
        if "pull_request" not in issue and issue["title"] == TITLE
    ]
    if len(issues) > 1:
        raise ValueError("Expected at most one open upstream sync status issue")
    previous = previous_events(issues[0].get("body") or "") if issues else []
    events = sorted(
        previous + read_events(status_dir),
        key=lambda event: event["time"],
        reverse=True,
    )[:LIMIT]
    body = render(pin, events)
    if issues:
        issue = gh(
            f"repos/{repository}/issues/{issues[0]['number']}",
            data={"body": body},
            method="PATCH",
        )
    else:
        issue = gh(
            f"repos/{repository}/issues",
            data={"title": TITLE, "body": body, "labels": [LABEL]},
            method="POST",
        )
    print(issue["html_url"])


def failures() -> None:
    results = json.loads(os.environ["RESULTS"])
    url = os.environ.get("RUN_URL") or None
    for job, outcome in sorted(results.items()):
        if outcome["result"] in {"failure", "cancelled"}:
            record("failure", f"Job {job} ended with {outcome['result']}", url)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    publisher = sub.add_parser("publish")
    publisher.add_argument("--repository", required=True)
    publisher.add_argument("--status-dir", type=Path, required=True)
    sub.add_parser("failures")
    args = parser.parse_args()
    try:
        if args.command == "publish":
            publish(args.repository, args.status_dir)
        else:
            failures()
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        print(f"engine status stopped: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
