#!/usr/bin/env python3
"""Prepare reproducible channel builds and publish complete GitHub releases."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess


def build_identity(branch, run, attempt):
    if branch not in ("dev", "main"):
        raise ValueError("Noches releases must build dev or main")
    if int(run) <= 0 or int(attempt) <= 0:
        raise ValueError("Build numbers must be positive")
    channel = "stable" if branch == "main" else "dev"
    # Workflow run numbers increase across both branches. Stable patch numbers
    # are CI-owned; a rerun gets its own version rather than replacing assets.
    version = f"0.1.{int(run)}" if channel == "stable" else f"0.1.{int(run)}-dev.{int(attempt)}"
    return channel, version


def manifest_for(directory, repository, channel, version, commit):
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ValueError("Invalid GitHub repository")
    required = [f"noches-{version}-{target}" for target in (
        "linux-x86_64.tar.gz", "linux-aarch64.tar.gz",
        "macos-arm64.dmg", "macos-arm64-app.tar.gz")]
    if not all((directory / name).is_file() for name in required):
        raise ValueError("Cannot publish an incomplete platform release")
    root = f"https://github.com/{repository}/releases"
    return dict(product="noches", channel=channel, version=version, commit=commit,
                notes_url=f"{root}/tag/v{version}", files={
                    name: dict(sha256=hashlib.sha256((directory / name).read_bytes()).hexdigest(),
                               size=(directory / name).stat().st_size,
                               url=f"{root}/download/v{version}/{name}") for name in required})


def gh(*args, **kwargs):
    return subprocess.check_output(["gh", *args], text=True, **kwargs).strip()


def release_for(repo, tag):
    try:
        return json.loads(gh("api", f"repos/{repo}/releases/tags/{tag}", stderr=subprocess.PIPE))
    except subprocess.CalledProcessError as error:
        if "404" in error.stderr:
            return None
        raise


def version_order(version):
    match = re.fullmatch(r"(\d+)\.(\d+)\.(\d+)(?:-dev\.(\d+))?", version)
    if not match:
        raise ValueError("Invalid channel version")
    return tuple(int(n or 0) for n in match.groups())


def publish(directory):
    repo = os.environ["GITHUB_REPOSITORY"]
    channel = os.environ["NOCHES_CHANNEL"]
    version = os.environ["NOCHES_VERSION"]
    commit = os.environ["GITHUB_SHA"]
    # Reject obsolete queued jobs before changing a channel. Actions concurrency
    # prevents concurrent publishers; checking the branch also handles reruns.
    head = gh("api", f"repos/{repo}/git/ref/heads/{os.environ['GITHUB_REF_NAME']}", "--jq", ".object.sha")
    if head != commit:
        print("Branch advanced during this build; leaving the current update feed unchanged.")
        return
    manifest = manifest_for(directory, repo, channel, version, commit)
    path = directory / "manifest.json"
    path.write_text(json.dumps(manifest, indent=2) + "\n")
    tag = f"v{version}"
    # A completed version is immutable. A failed draft can be safely resumed.
    existing = release_for(repo, tag)
    files = [str(directory / name) for name in manifest['files']] + [str(path)]
    if existing and not existing['draft']:
        saved = json.loads(gh("release", "download", tag, "--repo", repo, "--pattern", "manifest.json", "--output", "-"))
        if saved != manifest:
            raise ValueError("Refusing to overwrite an already published version. Start a new workflow run.")
    else:
        if not existing:
            gh("release", "create", tag, "--repo", repo, "--target", commit, "--draft", "--title",
               f"Noches {'Dev ' if channel == 'dev' else ''}{version}", "--generate-notes")
        gh("release", "upload", tag, *files, "--repo", repo, "--clobber")
        gh("release", "edit", tag, "--repo", repo, "--draft=false",
           f"--prerelease={'true' if channel == 'dev' else 'false'}", f"--latest={'true' if channel == 'stable' else 'false'}")
    # This release holds only the moving manifest. Its payload URLs always point
    # at the immutable version above. Mark both feed releases as prereleases.
    feed = f"noches-{channel}"
    feed_release = release_for(repo, feed)
    if feed_release and any(asset["name"] == "manifest.json" for asset in feed_release["assets"]):
        current = json.loads(gh("release", "download", feed, "--repo", repo, "--pattern", "manifest.json", "--output", "-"))
        if version_order(current['version']) > version_order(version):
            print("A newer build is already published; leaving the channel unchanged.")
            return
    if gh("api", f"repos/{repo}/git/ref/heads/{os.environ['GITHUB_REF_NAME']}", "--jq", ".object.sha") != commit:
        print("Branch advanced before publication; keeping the previous feed.")
        return
    if not feed_release:
        gh("release", "create", feed, "--repo", repo, "--target", commit, "--prerelease", "--latest=false",
           "--title", f"Noches {channel} update feed", "--notes", "Update metadata. Download installers from a versioned release.")
    gh("release", "upload", feed, str(path), "--repo", repo, "--clobber")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=["prepare", "publish"])
    parser.add_argument("--directory", type=Path, default=Path("artifacts"))
    args = parser.parse_args()
    if args.action == "prepare":
        channel, version = build_identity(os.environ.get("NOCHES_SOURCE_BRANCH", os.environ["GITHUB_REF_NAME"]), os.environ["GITHUB_RUN_NUMBER"], os.environ["GITHUB_RUN_ATTEMPT"])
        with open(os.environ["GITHUB_OUTPUT"], "a") as output:
            output.write(f"channel={channel}\nversion={version}\n")
    else:
        publish(args.directory)


if __name__ == "__main__":
    main()
