#!/usr/bin/env python3
"""Tag main for release; CI tests it and publishes the release, notes and DMG."""
import argparse
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
VERSION = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")


def parse_version(value):
    value = value.removeprefix("v")
    if not VERSION.fullmatch(value):
        raise ValueError("Use a version such as 0.2.0 or v0.2.0.")
    return tuple(map(int, value.split(".")))


def version_string(version):
    return ".".join(map(str, version))


def latest_version(refs):
    versions = []
    for ref in refs:
        tag = ref["ref"].removeprefix("refs/tags/")
        if tag.startswith("v") and VERSION.fullmatch(tag[1:]):
            versions.append(parse_version(tag))
    return max(versions, default=None)


def command(*arguments):
    return subprocess.run(
        arguments, cwd=ROOT, check=True, stdout=subprocess.PIPE, text=True
    ).stdout.strip()


def gh(*arguments):
    return command("gh", *arguments)


def pages(repository, endpoint):
    return [
        item
        for page in json.loads(gh("api", "--paginate", "--slurp", f"repos/{repository}/{endpoint}"))
        for item in page
    ]


def create_release(repository, remote, version, refs):
    previous = latest_version(refs)
    tag = f"v{version_string(version)}"
    if any(ref["ref"] == f"refs/tags/{tag}" for ref in refs):
        raise ValueError(f"{tag} already exists. Choose a new version; retry failed builds in Actions.")
    if previous is not None and version <= previous:
        raise ValueError(f"Choose a version newer than v{version_string(previous)}.")

    # Pin main before creating the tag, regardless of the local checkout or branch.
    command("git", "fetch", "--no-tags", remote, "refs/heads/main")
    commit = command("git", "rev-parse", "FETCH_HEAD")
    print(f"Releasing {tag} from {repository} main ({commit[:7]})…", flush=True)
    # Push just this tag, without changing branches or creating a local tag. A
    # normal Git push triggers Actions; tags created by a workflow token don't.
    command("git", "push", remote, f"{commit}:refs/tags/{tag}")
    print(f"https://github.com/{repository}/actions/workflows/release.yml")
    print("CI will test this commit, publish the release and DMG, and update installed apps.")
    print(f"Release notes will be at https://github.com/{repository}/releases/tag/{tag}")


def main():
    parser = argparse.ArgumentParser(prog="scripts/release.sh", description=__doc__)
    parser.add_argument("version", nargs="?", help="Version to release; otherwise prompts with a suggestion")
    arguments = parser.parse_args()
    version = parse_version(arguments.version) if arguments.version else None
    if shutil.which("gh") is None:
        raise ValueError("Install the GitHub CLI and sign in with gh auth login first.")
    remote = command("git", "remote", "get-url", "--push", "origin")
    repository = gh("repo", "view", remote, "--json", "nameWithOwner", "--jq", ".nameWithOwner")
    refs = pages(repository, "git/matching-refs/tags/v")
    previous = latest_version(refs)
    if version is None:
        if previous is None:
            manifest = (ROOT / "crates/app/Cargo.toml").read_text()
            suggested = re.search(r'^version = "([^"]+)"$', manifest, re.MULTILINE).group(1)
        else:
            suggested = version_string((*previous[:2], previous[2] + 1))
        print(f"This publishes the current {repository} main; local changes are not included.")
        version = parse_version(input(f"Release version [{suggested}]: ").strip() or suggested)
    create_release(repository, remote, version, refs)


if __name__ == "__main__":
    try:
        main()
    except (ValueError, EOFError, subprocess.CalledProcessError) as error:
        print(f"Release failed: {error}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        sys.exit(130)
