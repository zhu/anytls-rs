#!/usr/bin/env python3
"""Preview or publish the reviewed client issues using an authenticated gh CLI."""

import argparse
import json
from pathlib import Path
import subprocess
import tempfile


def gh_json(*args):
    result = subprocess.run(
        ["gh", *args], check=True, capture_output=True, text=True
    )
    return json.loads(result.stdout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--apply", action="store_true", help="Create missing issues")
    args = parser.parse_args()
    manifest = json.loads(Path(__file__).with_name("issues.json").read_text())
    repository = "zhu/anytls-rs"
    if manifest["repository"] != repository:
        raise SystemExit("Unexpected repository in issue manifest")

    if not args.apply:
        for issue in manifest["issues"]:
            print(f"{issue['priority']}  {issue['title']}")
        print(f"Preview only: {len(manifest['issues'])} drafts for {repository}.")
        return

    metadata = gh_json("api", f"repos/{repository}")
    if not metadata["has_issues"]:
        raise SystemExit("Issues are disabled; enable them before publishing.")
    pages = gh_json(
        "api", "--paginate", "--slurp",
        f"repos/{repository}/issues?state=all&per_page=100",
    )
    existing = [item for page in pages for item in page if "pull_request" not in item]

    for issue in manifest["issues"]:
        marker = f"<!-- anytls-client-audit: {issue['id']} -->"
        matches = [
            item for item in existing
            if item["title"] == issue["title"] or marker in (item.get("body") or "")
        ]
        if matches:
            print(f"Already exists: {matches[0]['html_url']}")
            continue
        with tempfile.TemporaryDirectory(prefix="anytls-issue-") as directory:
            body_file = Path(directory) / "body.md"
            body_file.write_text(issue["body"], encoding="utf-8")
            result = subprocess.run(
                ["gh", "issue", "create", "--repo", repository,
                 "--title", issue["title"], "--body-file", str(body_file)],
                check=True, capture_output=True, text=True,
            )
            print(result.stdout.strip(), flush=True)


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        raise SystemExit(error.stderr.strip() or str(error)) from error
