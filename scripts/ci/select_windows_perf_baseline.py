#!/usr/bin/env python3
"""Select the exact main attempt that supplies the Windows perf trend baseline."""

from datetime import datetime, timedelta, timezone
from functools import partial
import json
import os
from pathlib import Path
import re
import subprocess
from time import monotonic
from urllib.parse import urlencode

WINDOWS_JOB = "Windows performance budget"
CONSTRAINED_JOB = "Constrained-envelope budget (llvmpipe / 2 CPUs)"
TIMING_JOB = "Linux timing assertions"
PUBLISH_STEP = "Publish main-pinned Windows perf baseline"
BASELINE = "windows-perf-baseline-main"
COMPLETE_CONCLUSIONS = ("success", "failure", "timed_out")
COMMAND_TIMEOUT_SECONDS = 15
LOOKUP_TIMEOUT_SECONDS = 60


class LookupUnavailable(Exception):
    def __init__(self, stage, reason):
        self.stage = stage
        self.reason = reason


def github_rows(route, key, *, deadline):
    started = monotonic()
    for attempt in range(2):
        remaining = deadline - monotonic()
        if remaining <= 0:
            raise LookupUnavailable(key, "deadline")
        try:
            output = subprocess.check_output(
                ["gh", "api", "--method", "GET", "--paginate", "--slurp", route],
                encoding="utf-8",
                stderr=subprocess.DEVNULL,
                timeout=min(COMMAND_TIMEOUT_SECONDS, remaining),
            )
            break
        except subprocess.TimeoutExpired:
            now = monotonic()
            if attempt or now >= deadline:
                raise LookupUnavailable(key, "timeout") from None
            print(
                f"Windows perf baseline lookup retry. stage={key} reason=timeout "
                f"elapsed={now - started:.3f}s"
            )
        except subprocess.SubprocessError:
            raise LookupUnavailable(key, "provider-exit") from None
        except UnicodeDecodeError:
            raise LookupUnavailable(key, "encoding") from None
        except OSError:
            raise LookupUnavailable(key, "process-io") from None

    try:
        pages = json.loads(output)
    except (ValueError, RecursionError):
        raise LookupUnavailable(key, "json") from None
    if not isinstance(pages, list) or not pages or any(
        not isinstance(page, dict) or not isinstance(page.get(key), list) for page in pages
    ):
        raise LookupUnavailable(key, "response-shape")
    rows = [row for page in pages for row in page[key]]
    if not all(isinstance(row, dict) for row in rows):
        raise LookupUnavailable(key, "response-shape")
    return rows


def positive_int(value):
    return type(value) is int and value > 0


def same_main_run(run, repo_id, current_run):
    return (
        positive_int(run.get("id"))
        and run["id"] != current_run
        and positive_int(run.get("run_attempt"))
        and run.get("event") in ("schedule", "workflow_dispatch")
        and run.get("status") == "completed"
        and run.get("conclusion") in COMPLETE_CONCLUSIONS
        and run.get("head_branch") == "main"
        and (run.get("repository") or {}).get("id") == repo_id
        and (run.get("head_repository") or {}).get("id") == repo_id
        and run.get("path") == ".github/workflows/perf-budget.yml"
        and isinstance(run.get("head_sha"), str)
        and re.fullmatch(r"[0-9a-f]{40}", run["head_sha"]) is not None
        and isinstance(run.get("created_at"), str)
    )


def successful(job):
    return job.get("status") == "completed" and job.get("conclusion") == "success"


def upload_time_matches(artifact, publish_step):
    try:
        def utc_time(raw):
            return datetime.fromisoformat(raw.replace("Z", "+00:00"))

        start = utc_time(publish_step["started_at"])
        created = utc_time(artifact["created_at"])
        end = utc_time(publish_step["completed_at"])
        return all(time.utcoffset() is not None for time in (start, created, end)) and start <= created <= end
    except (AttributeError, KeyError, TypeError, ValueError):
        return False


def select_baseline(repo, repo_id, current_run, get_rows=None):
    if get_rows is None:
        get_rows = partial(github_rows, deadline=monotonic() + LOOKUP_TIMEOUT_SECONDS)
    root = f"repos/{repo}/actions"
    # Include runs created just before the 90-day artifact retention window.
    since = datetime.now(timezone.utc).date() - timedelta(days=91)
    query = urlencode(
        {"branch": "main", "status": "completed", "created": f">={since}", "per_page": 100}
    )
    runs = get_rows(f"{root}/workflows/perf-budget.yml/runs?{query}", "workflow_runs")
    candidates = sorted(
        (run for run in runs if same_main_run(run, repo_id, current_run)),
        key=lambda run: (run["created_at"], run["id"]),
        reverse=True,
    )
    for run in candidates:
        run_id = run["id"]
        run_route = f"{root}/runs/{run_id}"
        artifacts = None
        for attempt in range(run["run_attempt"], 0, -1):
            jobs = get_rows(f"{run_route}/attempts/{attempt}/jobs?per_page=100", "jobs")
            by_name = {job.get("name"): job for job in jobs}
            expected = {WINDOWS_JOB, CONSTRAINED_JOB}
            if len(by_name) != len(jobs) or set(by_name) not in (expected, expected | {TIMING_JOB}):
                continue
            if any(
                job.get("run_id") != run_id
                or job.get("run_attempt") != attempt
                or job.get("head_sha") != run["head_sha"]
                for job in jobs
            ):
                continue
            if not all(successful(by_name[name]) for name in expected):
                continue

            timing = by_name.get(TIMING_JOB)
            legacy = timing is None
            if legacy:
                if attempt != 1:
                    continue
                artifact_name = BASELINE
            else:
                if (
                    timing.get("status") != "completed"
                    or timing.get("conclusion") not in COMPLETE_CONCLUSIONS
                ):
                    continue
                artifact_name = f"{BASELINE}-attempt-{attempt}"

            windows = by_name[WINDOWS_JOB]
            publish_steps = [
                step for step in windows.get("steps", []) if step.get("name") == PUBLISH_STEP
            ]
            if len(publish_steps) != 1 or not successful(publish_steps[0]):
                continue
            if artifacts is None:
                artifacts = get_rows(f"{run_route}/artifacts?per_page=100", "artifacts")
            matches = [artifact for artifact in artifacts if artifact.get("name") == artifact_name]
            if len(matches) != 1:
                continue
            artifact = matches[0]
            digest = artifact.get("digest")
            link = artifact.get("workflow_run") or {}
            identity = {
                "id": run_id,
                "repository_id": repo_id,
                "head_repository_id": repo_id,
                "head_branch": "main",
                "head_sha": run["head_sha"],
            }
            if (
                not positive_int(artifact.get("id"))
                or artifact.get("expired") is not False
                or not isinstance(digest, str)
                or re.fullmatch(r"sha256:[0-9a-f]{64}", digest) is None
                or any(link.get(key) != value for key, value in identity.items())
                or not upload_time_matches(artifact, publish_steps[0])
            ):
                continue
            return run_id, attempt, artifact["id"]
    return None


def unavailable(stage, reason, started):
    print(
        f"::notice::Windows perf baseline lookup unavailable. stage={stage} reason={reason} "
        f"elapsed={monotonic() - started:.3f}s. No baseline downloaded."
    )


def main():
    started = monotonic()
    stage = "configuration"
    try:
        repo = os.environ["GITHUB_REPOSITORY"]
        repo_id = int(os.environ["GITHUB_REPOSITORY_ID"])
        current_run = int(os.environ["GITHUB_RUN_ID"])
        stage = "selection"
        choice = select_baseline(repo, repo_id, current_run)
        if choice is not None:
            stage = "output"
            run_id, attempt, artifact_id = choice
            with Path(os.environ["GITHUB_OUTPUT"]).open("a", encoding="utf-8") as output:
                output.write(f"run-id={run_id}\nartifact-id={artifact_id}\n")
    except LookupUnavailable as error:
        unavailable(error.stage, error.reason, started)
        return
    except (ValueError, KeyError, TypeError, AttributeError):
        unavailable(stage, "invalid-data", started)
        return
    except OSError:
        unavailable(stage, "io", started)
        return
    if choice is None:
        print(
            "::notice::No eligible main Windows perf baseline found. "
            f"elapsed={monotonic() - started:.3f}s"
        )
        return
    print(
        f"Windows perf baseline run {run_id} attempt {attempt}, artifact {artifact_id}. "
        f"elapsed={monotonic() - started:.3f}s"
    )


if __name__ == "__main__":
    main()
