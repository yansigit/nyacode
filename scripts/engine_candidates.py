#!/usr/bin/env python3
"""Propose pinned engine releases and promote only exact, fully qualified heads."""

from __future__ import annotations

import argparse
import base64
import hashlib
import io
import json
import os
import re
import subprocess
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = "engine-qualification.yml"
SHA = re.compile(r"[0-9a-f]{40}")
TAG = re.compile(r"rust-v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)")
MUTABLE_LOCK_FIELDS = {"revision", "release_tag", "archive_sha256", "toolchain"}
TARGETS = (
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
)
REQUIRED_JOBS = {
    "Dependency policy / cargo-deny",
    "Engine controls",
    "Standalone providers",
    "Release tooling and version",
    "Native provider checks / Linux architecture, tests, and features",
    "SDKs against the packaged CLI",
    "Verify complete release set",
    "Engine acceptance",
    *(f"Package {target}" for target in TARGETS),
}


def select_mode(paths: list[str], candidate_branch: str = "") -> str:
    """Unknown inputs, shared dependencies and engine changes qualify every target."""
    if candidate_branch.startswith("engine/"):
        if not TAG.fullmatch(candidate_branch.removeprefix("engine/")) or paths != [
            "engine.lock"
        ]:
            raise ValueError("Engine candidate branches may change only engine.lock")
        return "full"
    mode = "controls"
    for path in paths:
        if path.startswith("crates/providers/provider-") and Path(path).name not in {
            "Cargo.toml",
            "Cargo.lock",
            "BUILD.bazel",
            "build.rs",
        }:
            mode = "providers"
        elif (
            path.startswith("docs/")
            and not path.startswith("docs/native-providers/")
            or path in {"README.md", "scripts/tests/test_engine_candidates.py"}
        ):
            continue
        else:
            return "full"
    return mode if paths else "full"


def validate_lock(original: dict, candidate: dict, tag: str, revision: str) -> None:
    if not TAG.fullmatch(tag) or not SHA.fullmatch(revision):
        raise ValueError("Expected a stable release tag and full upstream revision")
    if original.keys() != candidate.keys() or any(
        type(candidate[key]) is not type(value) or candidate[key] != value
        for key, value in original.items()
        if key not in MUTABLE_LOCK_FIELDS
    ):
        raise ValueError("Candidate changes the engine integration contract")
    if candidate["revision"] != revision or candidate["release_tag"] != tag:
        raise ValueError("Candidate pin does not match the stable upstream release")
    if not re.fullmatch(r"[0-9a-f]{64}", candidate.get("archive_sha256", "")):
        raise ValueError("Candidate archive digest must be SHA-256")
    if not re.fullmatch(r"\d+\.\d+\.\d+", candidate.get("toolchain", "")):
        raise ValueError("Candidate toolchain must be a pinned stable version")


def validate_jobs(jobs: list[dict], head: str) -> None:
    names = [job["name"] for job in jobs]
    if len(names) != len(set(names)) or not REQUIRED_JOBS <= set(names):
        raise ValueError("Missing or duplicated required qualification jobs")
    for job in jobs:
        if (
            job["status"] != "completed"
            or job["conclusion"] != "success"
            or job.get("head_sha") != head
        ):
            raise ValueError(
                f"Qualification job did not succeed for this head: {job['name']}"
            )


def validate_protection(protection: dict) -> None:
    required = protection.get("requiredStatusChecks", [])
    if (
        protection.get("requiresStrictStatusChecks") is not True
        or not any(
            check.get("context") == "Engine acceptance"
            and (check.get("app") or {}).get("databaseId") == 15368
            for check in required
        )
        or protection.get("isAdminEnforced") is not True
        or protection.get("bypassPullRequestAllowances", {}).get("totalCount") != 0
        or protection.get("allowsForcePushes") is not False
        or protection.get("allowsDeletions") is not False
    ):
        raise ValueError(
            "Automatic promotion requires strict protected Engine acceptance with no bypass"
        )


def validate_promotion(
    run: dict,
    pr: dict,
    report: dict,
    jobs: list[dict],
    current_base: str,
    workflow_id: int,
    repository: str,
    changed_files: list[dict],
    parents: list[str],
) -> None:
    head = pr["head"]["sha"]
    tag = report.get("release_tag", "")
    if (
        not SHA.fullmatch(head)
        or not SHA.fullmatch(current_base)
        or not TAG.fullmatch(tag)
    ):
        raise ValueError("Invalid qualification identity")
    if (
        run["workflow_id"] != workflow_id
        or run["event"] != "workflow_dispatch"
        or run["path"] != f".github/workflows/{WORKFLOW}"
        or run["status"] != "completed"
        or run["conclusion"] != "success"
        or run["head_sha"] != head
        or run["head_repository"]["full_name"] != repository
    ):
        raise ValueError(
            "Qualification run is not successful for the expected workflow/head"
        )
    if (
        pr["state"] != "open"
        or pr["draft"]
        or pr["head"]["repo"]["full_name"] != repository
        or pr["head"]["ref"] != f"engine/{tag}"
        or pr["base"]["sha"] != current_base
        or parents != [current_base]
    ):
        raise ValueError("Candidate is stale, foreign, or has unexpected commits")
    if (
        type(report.get("schema_version")) is not int
        or report.get("schema_version") != 1
        or any(
            type(report.get(key)) is not int
            for key in ("pull_request", "run_id", "run_attempt")
        )
        or report.get("head_sha") != head
        or report.get("base_sha") != current_base
        or report.get("pull_request") != pr["number"]
        or report.get("run_id") != run["id"]
        or report.get("run_attempt") != run["run_attempt"]
        or report.get("results")
        != {
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
        }
    ):
        raise ValueError("Qualification report identity/results do not match")
    if (
        len(changed_files) != 1
        or changed_files[0].get("filename") != "engine.lock"
        or changed_files[0].get("status") != "modified"
    ):
        raise ValueError("Automatic candidates may change only engine.lock")
    validate_jobs(jobs, head)


def command(args: list[str], *, data: dict | None = None, raw: bool = False):
    result = subprocess.run(
        args,
        cwd=ROOT,
        input=json.dumps(data).encode() if data is not None else None,
        capture_output=True,
        check=True,
    )
    return result.stdout if raw else json.loads(result.stdout)


def api(path: str, *, data: dict | None = None):
    args = ["gh", "api", path]
    if data is not None:
        args += ["--method", "POST", "--input", "-"]
    return command(args, data=data)


def pages(path: str, field: str | None = None) -> list:
    result = command(["gh", "api", "--paginate", "--slurp", path])
    return [item for page in result for item in (page[field] if field else page)]


def stable_release(tag: str, upstream: str) -> str:
    if not TAG.fullmatch(tag):
        raise ValueError("Only rust-vX.Y.Z stable releases are eligible")
    release = api(f"repos/{upstream}/releases/tags/{tag}")
    if release["draft"] or release["prerelease"] or release["tag_name"] != tag:
        raise ValueError("Draft and prerelease upstream releases are ineligible")
    ref = api(f"repos/{upstream}/git/ref/tags/{tag}")["object"]
    for _ in range(5):
        if ref["type"] == "commit" and SHA.fullmatch(ref["sha"]):
            return ref["sha"]
        if ref["type"] != "tag":
            break
        ref = api(f"repos/{upstream}/git/tags/{ref['sha']}")["object"]
    raise ValueError("Release tag does not resolve to one full commit SHA")


def lock_at(repository: str, revision: str) -> dict:
    content = api(f"repos/{repository}/contents/engine.lock?ref={revision}")
    if (
        content["type"] != "file"
        or content["encoding"] != "base64"
        or content["size"] > 65536
    ):
        raise ValueError("Unexpected engine.lock content")
    return json.loads(base64.b64decode(content["content"]))


def report_for(repository: str, run_id: int) -> dict:
    artifacts = pages(
        f"repos/{repository}/actions/runs/{run_id}/artifacts?per_page=100", "artifacts"
    )
    selected = [
        a for a in artifacts if a["name"] == "engine-qualification" and not a["expired"]
    ]
    if len(selected) != 1 or selected[0].get("size_in_bytes", 0) > 1048576:
        raise ValueError("Expected exactly one qualification artifact")
    archive = command(
        ["gh", "api", f"repos/{repository}/actions/artifacts/{selected[0]['id']}/zip"],
        raw=True,
    )
    # Read one bounded JSON member; never extract or execute artifact contents.
    with zipfile.ZipFile(io.BytesIO(archive)) as stream:
        if (
            stream.namelist() != ["qualification.json"]
            or stream.infolist()[0].file_size > 65536
        ):
            raise ValueError("Unexpected qualification artifact contents")
        return json.loads(stream.read("qualification.json"))


def current_base(repository: str, branch: str) -> str:
    return api(f"repos/{repository}/git/ref/heads/{branch}")["object"]["sha"]


def run_jobs(repository: str, run: dict) -> list[dict]:
    return pages(
        f"repos/{repository}/actions/runs/{run['id']}/attempts/{run['run_attempt']}/jobs?per_page=100",
        "jobs",
    )


def discover(args, repository: str, branch: str) -> None:
    original = json.loads((ROOT / "engine.lock").read_text())
    base = current_base(repository, branch)
    local = subprocess.check_output(
        ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
    ).strip()
    if base != local:
        raise ValueError("Default branch moved; retry discovery on its current head")
    if args.scheduled and os.environ.get("NYACODE_ENGINE_UPGRADES_ENABLED") != "true":
        print("Scheduled engine upgrades remain disabled pending cutover qualification")
        return
    releases = pages(f"repos/{original['repository']}/releases?per_page=100")
    tags = [
        r["tag_name"]
        for r in releases
        if not r["draft"] and not r["prerelease"] and TAG.fullmatch(r["tag_name"])
    ]
    tags.sort(
        key=lambda tag: tuple(map(int, TAG.fullmatch(tag).groups())), reverse=True
    )
    tag = args.tag or (tags[0] if tags else "")
    revision = stable_release(tag, original["repository"])
    if revision == original["revision"]:
        print("The latest stable engine release is already accepted")
        return
    comparison = api(
        f"repos/{original['repository']}/compare/{original['revision']}...{revision}"
    )
    if comparison["status"] != "ahead":
        print("The stable release is not newer than the accepted engine revision")
        return
    if (
        not args.tag
        and original.get("release_tag")
        and tuple(map(int, TAG.fullmatch(tag).groups()))
        <= tuple(map(int, TAG.fullmatch(original["release_tag"]).groups()))
    ):
        print("No newer stable engine release is available")
        return
    preflight = subprocess.run(
        [
            sys.executable,
            str(ROOT / "scripts/engine.py"),
            "upgrade",
            tag,
            "--mode",
            "controls",
        ],
        cwd=ROOT,
        check=False,
    )
    candidate = json.loads(
        (ROOT / ".engine/upgrades" / tag / "engine.lock").read_text()
    )
    validate_lock(original, candidate, tag, revision)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(
        json.dumps(
            {
                "schema_version": 1,
                "base_sha": base,
                "tag": tag,
                "revision": revision,
                "engine_lock": candidate,
                "prepared": preflight.returncode == 0,
                "diagnostics_url": discovery_url(repository),
            },
            indent=2,
        )
        + "\n"
    )


def publish(args, repository: str, branch: str) -> None:
    if args.proposal.stat().st_size > 65536:
        raise ValueError("Oversized proposal")
    proposal = json.loads(args.proposal.read_text())
    base = current_base(repository, branch)
    if proposal.get("schema_version") != 1 or proposal.get("base_sha") != base:
        raise ValueError("Proposal base is stale")
    if type(proposal.get("prepared")) is not bool:
        raise ValueError("Proposal must record its preflight result")
    original = lock_at(repository, base)
    revision = stable_release(proposal["tag"], original["repository"])
    if proposal.get("revision") != revision:
        raise ValueError("Upstream release tag moved since discovery")
    validate_lock(original, proposal["engine_lock"], proposal["tag"], revision)
    candidate_branch = f"engine/{proposal['tag']}"
    existing = api(
        f"repos/{repository}/pulls?state=all&head={repository.split('/')[0]}:{candidate_branch}&per_page=100"
    )
    if existing and (len(existing) != 1 or existing[0]["state"] != "open"):
        print("Candidate was already closed; no branch was changed")
        return
    try:
        head = api(f"repos/{repository}/git/ref/heads/{candidate_branch}")["object"][
            "sha"
        ]
    except subprocess.CalledProcessError as error:
        if b"HTTP 404" not in (error.stderr or b""):
            raise
        head = None
    if head:
        prior = api(f"repos/{repository}/compare/{base}...{head}")
        files = prior["files"]
        if (
            prior["total_commits"] != 1
            or len(files) != 1
            or files[0]["filename"] != "engine.lock"
            or files[0]["status"] != "modified"
            or lock_at(repository, head) != proposal["engine_lock"]
        ):
            raise ValueError(
                "Existing candidate requires manual repair; its branch was not changed"
            )
        if existing:
            print(f"Unchanged candidate already published: {existing[0]['html_url']}")
            return
    else:
        commit = api(f"repos/{repository}/git/commits/{base}")
        content = json.dumps(proposal["engine_lock"], indent=2) + "\n"
        blob = api(
            f"repos/{repository}/git/blobs",
            data={"content": content, "encoding": "utf-8"},
        )
        tree = api(
            f"repos/{repository}/git/trees",
            data={
                "base_tree": commit["tree"]["sha"],
                "tree": [
                    {
                        "path": "engine.lock",
                        "mode": "100644",
                        "type": "blob",
                        "sha": blob["sha"],
                    }
                ],
            },
        )
        head = api(
            f"repos/{repository}/git/commits",
            data={
                "message": f"Qualify engine {proposal['tag']}",
                "tree": tree["sha"],
                "parents": [base],
            },
        )["sha"]
        api(
            f"repos/{repository}/git/refs",
            data={"ref": f"refs/heads/{candidate_branch}", "sha": head},
        )
    pr = (
        existing[0]
        if existing
        else api(
            f"repos/{repository}/pulls",
            data={
                "title": f"Qualify engine {proposal['tag']}",
                "head": candidate_branch,
                "base": branch,
                "body": (
                    f"Pin upstream {revision}. Full engine qualification is dispatched explicitly. "
                    "Only engine.lock is eligible for automatic promotion; any integration repair requires review.\n\n"
                    + (
                        "Preflight passed."
                        if proposal["prepared"]
                        else "Preflight failed. This draft requires targeted integration repair before promotion."
                    )
                    + (
                        f" [Preflight diagnostics]({discovery_url(repository)})."
                        if discovery_url(repository)
                        else ""
                    )
                ),
                "draft": not proposal["prepared"],
            },
        )
    )
    subprocess.run(
        [
            "gh",
            "workflow",
            "run",
            WORKFLOW,
            "--repo",
            repository,
            "--ref",
            candidate_branch,
            "-f",
            f"base_sha={base}",
            "-f",
            f"pull_request={pr['number']}",
            "-f",
            f"release_tag={proposal['tag']}",
        ],
        cwd=ROOT,
        check=True,
    )
    print(pr["html_url"])


def discovery_url(repository: str) -> str:
    run_id = os.environ.get("GITHUB_RUN_ID", "")
    return (
        f"https://github.com/{repository}/actions/runs/{run_id}"
        if run_id.isdecimal()
        else ""
    )


def observe(args, repository: str, branch: str) -> None:
    """Report completed runs using their branch/head, never an artifact's PR number."""
    run = api(f"repos/{repository}/actions/runs/{args.run_id}")
    workflow = api(f"repos/{repository}/actions/workflows/{WORKFLOW}")
    candidate_branch = run.get("head_branch", "")
    tag = candidate_branch.removeprefix("engine/")
    if (
        run["workflow_id"] != workflow["id"]
        or run["event"] != "workflow_dispatch"
        or run["path"] != f".github/workflows/{WORKFLOW}"
        or run["status"] != "completed"
        or run["head_repository"]["full_name"] != repository
        or not SHA.fullmatch(run["head_sha"])
        or candidate_branch != f"engine/{tag}"
        or not TAG.fullmatch(tag)
    ):
        print("No automatic candidate report for this qualification run")
        return
    prs = api(
        f"repos/{repository}/pulls?state=open&head={repository.split('/')[0]}:{candidate_branch}&per_page=100"
    )
    prs = [
        pr
        for pr in prs
        if pr["head"]["sha"] == run["head_sha"]
        and pr["head"]["ref"] == candidate_branch
        and pr["head"]["repo"]["full_name"] == repository
        and pr["base"]["ref"] == branch
    ]
    if len(prs) != 1:
        print("Qualification has no unique open candidate at this exact head")
        return
    pr = prs[0]
    conclusion = run["conclusion"]
    if conclusion not in {
        "success",
        "failure",
        "cancelled",
        "timed_out",
        "action_required",
        "neutral",
        "skipped",
        "stale",
        "startup_failure",
    }:
        conclusion = "incomplete"
    lines = [
        "<!-- engine-qualification:start -->",
        (
            f"Qualification **{conclusion}** for `{run['head_sha']}`: "
            f"[run {run['id']}, attempt {run['run_attempt']}](https://github.com/{repository}/actions/runs/{run['id']}/attempts/{run['run_attempt']})."
        ),
    ]
    jobs = run_jobs(repository, run)
    incomplete = sorted(
        REQUIRED_JOBS
        - {
            job["name"]
            for job in jobs
            if job["status"] == "completed"
            and job["conclusion"] == "success"
            and job.get("head_sha") == run["head_sha"]
        }
    )
    if incomplete:
        lines.append(
            "Required checks needing attention: " + ", ".join(incomplete) + "."
        )
    consider_promotion = conclusion == "success" and not incomplete
    if conclusion == "success":
        try:
            report_for(repository, run["id"])
        except (ValueError, zipfile.BadZipFile):
            lines.append(
                "Qualification report is missing or invalid; automatic promotion is blocked."
            )
            consider_promotion = False
    latest = api(f"repos/{repository}/pulls/{pr['number']}")
    if (
        latest["state"] != "open"
        or latest["head"]["sha"] != run["head_sha"]
        or latest["head"]["ref"] != candidate_branch
        or latest["head"]["repo"]["full_name"] != repository
        or latest["base"]["ref"] != branch
    ):
        print("Candidate moved while observing qualification; no report was changed")
        return
    if not consider_promotion and not latest["draft"]:
        subprocess.run(
            ["gh", "pr", "ready", str(pr["number"]), "--undo", "--repo", repository],
            cwd=ROOT,
            check=True,
        )
        latest["draft"] = True
    consider_promotion = consider_promotion and not latest["draft"]
    if latest["draft"]:
        lines.append("Draft candidate requires targeted repair before promotion.")
    lines.append("<!-- engine-qualification:end -->")
    body = latest.get("body") or ""
    marker = re.compile(
        r"<!-- engine-qualification:start -->.*?<!-- engine-qualification:end -->",
        re.DOTALL,
    )
    body = marker.sub("", body).rstrip() + "\n\n" + "\n".join(lines)
    command(
        [
            "gh",
            "api",
            f"repos/{repository}/pulls/{pr['number']}",
            "--method",
            "PATCH",
            "--input",
            "-",
        ],
        data={"body": body},
    )
    if args.github_output:
        with args.github_output.open("a") as output:
            output.write(
                f"consider_promotion={'true' if consider_promotion else 'false'}\n"
            )


def promote(args, repository: str, branch: str) -> None:
    run = api(f"repos/{repository}/actions/runs/{args.run_id}")
    workflow = api(f"repos/{repository}/actions/workflows/{WORKFLOW}")
    report = report_for(repository, args.run_id)
    number = report.get("pull_request")
    if not isinstance(number, int) or isinstance(number, bool) or number <= 0:
        print("Rehearsal qualification has no automatic candidate to promote")
        return
    pr = api(f"repos/{repository}/pulls/{number}")
    base = current_base(repository, branch)
    if pr["base"]["ref"] != branch:
        raise ValueError("Candidate targets an unexpected base branch")
    files = pages(f"repos/{repository}/pulls/{number}/files?per_page=100")
    parents = [
        p["sha"]
        for p in api(f"repos/{repository}/git/commits/{pr['head']['sha']}")["parents"]
    ]
    validate_promotion(
        run,
        pr,
        report,
        run_jobs(repository, run),
        base,
        workflow["id"],
        repository,
        files,
        parents,
    )
    original = lock_at(repository, base)
    candidate = lock_at(repository, run["head_sha"])
    revision = stable_release(report["release_tag"], original["repository"])
    validate_lock(original, candidate, report["release_tag"], revision)
    if (
        report.get("engine_lock_sha256")
        != hashlib.sha256((json.dumps(candidate, indent=2) + "\n").encode()).hexdigest()
    ):
        raise ValueError("Qualified engine lock differs from the candidate pin")
    owner, name = repository.split("/")
    protected = api(
        "graphql",
        data={
            "query": "query($owner:String!,$name:String!,$ref:String!){repository(owner:$owner,name:$name){ref(qualifiedName:$ref){branchProtectionRule{isAdminEnforced requiresStrictStatusChecks requiredStatusChecks{context app{databaseId}} allowsForcePushes allowsDeletions bypassPullRequestAllowances(first:1){totalCount}}}}}",
            "variables": {"owner": owner, "name": name, "ref": f"refs/heads/{branch}"},
        },
    )
    validate_protection(
        protected["data"]["repository"]["ref"]["branchProtectionRule"] or {}
    )
    latest = api(f"repos/{repository}/pulls/{number}")
    if (
        latest["head"]["sha"] != run["head_sha"]
        or latest["base"]["sha"] != base
        or latest["state"] != "open"
        or latest["draft"]
        or latest.get("mergeable_state") != "clean"
    ):
        raise ValueError(
            "Candidate is not immediately mergeable at its qualified head/base"
        )
    if current_base(repository, branch) != base:
        raise ValueError("Default branch moved during promotion checks")
    # Merge immediately; never queue approval that a later head could inherit.
    subprocess.run(
        [
            "gh",
            "pr",
            "merge",
            str(number),
            "--repo",
            repository,
            "--squash",
            "--match-head-commit",
            run["head_sha"],
        ],
        cwd=ROOT,
        check=True,
    )


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    select = sub.add_parser("select")
    select.add_argument("--paths-file", type=Path, required=True)
    select.add_argument("--github-output", type=Path)
    select.add_argument("--candidate-branch", default="")
    select.add_argument("--base", default="")
    discovery = sub.add_parser("discover")
    discovery.add_argument("--tag", default="")
    discovery.add_argument("--scheduled", action="store_true")
    discovery.add_argument("--output", type=Path, required=True)
    writer = sub.add_parser("publish")
    writer.add_argument("--proposal", type=Path, required=True)
    promotion = sub.add_parser("promote")
    promotion.add_argument("--run-id", type=int, required=True)
    observation = sub.add_parser("observe")
    observation.add_argument("--run-id", type=int, required=True)
    observation.add_argument("--github-output", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "select":
            mode = select_mode(
                args.paths_file.read_text().splitlines(), args.candidate_branch
            )
            if args.candidate_branch.startswith("engine/"):
                if not SHA.fullmatch(args.base):
                    raise ValueError(
                        "Engine candidate acceptance needs an exact base SHA"
                    )
                original = json.loads(
                    subprocess.check_output(
                        ["git", "show", f"{args.base}:engine.lock"], cwd=ROOT
                    )
                )
                tag = args.candidate_branch.removeprefix("engine/")
                validate_lock(
                    original,
                    json.loads((ROOT / "engine.lock").read_text()),
                    tag,
                    stable_release(tag, original["repository"]),
                )
            print(mode)
            if args.github_output:
                with args.github_output.open("a") as output:
                    output.write(f"mode={mode}\n")
        else:
            repository = os.environ["GITHUB_REPOSITORY"]
            branch = api(f"repos/{repository}")["default_branch"]
            {
                "discover": discover,
                "publish": publish,
                "observe": observe,
                "promote": promote,
            }[args.command](args, repository, branch)
    except (
        ValueError,
        KeyError,
        TypeError,
        OSError,
        subprocess.CalledProcessError,
    ) as error:
        print(f"engine candidate stopped: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
