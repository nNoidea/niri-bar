#!/usr/bin/env python3
"""Generate categorized changelog from git commit messages based on simple keywords."""

import os
import re
import subprocess
import sys


def get_git_repo() -> str | None:
    """Attempt to detect github 'owner/repo' string."""
    env_repo = os.environ.get("GITHUB_REPOSITORY")
    if env_repo:
        return env_repo.strip()

    try:
        remote = subprocess.check_output(
            ["git", "remote", "get-url", "origin"],
            stderr=subprocess.DEVNULL,
        ).decode("utf-8").strip()
        match = re.search(r"github\.com[:/]([^/]+/[^/.]+?)(?:\.git)?$", remote)
        if match:
            return match.group(1)
    except Exception:
        pass
    return None


def categorize_commit(subject: str) -> str:
    """Categorize commit message by keyword into: 'feat', 'fix', 'doc', or 'chore'."""
    lower = subject.lower()

    # Bug fixes
    if re.search(r"\b(fix|fixes|fixed|bug|bugs|hotfix|patch)\b", lower):
        return "fix"

    # Features
    if re.search(r"\b(feat|feature|features|add|added|implement|implemented)\b", lower):
        return "feat"

    # Documentation
    if re.search(r"\b(doc|docs|documentation|readme)\b", lower):
        return "doc"

    # Fallback to chore for everything else
    return "chore"


def get_commits(prev_tag: str | None, current_ref: str = "HEAD") -> list[tuple[str, str]]:
    """Fetch list of non-merge (short_hash, subject) commits between prev_tag and current_ref."""
    if prev_tag:
        git_range = f"{prev_tag}..{current_ref}"
    else:
        git_range = current_ref

    try:
        out = subprocess.check_output(
            ["git", "log", git_range, "--no-merges", "--pretty=format:%h|%s"],
            stderr=subprocess.DEVNULL,
        ).decode("utf-8")
    except subprocess.CalledProcessError:
        return []

    commits = []
    for line in out.splitlines():
        line = line.strip()
        if not line or "|" not in line:
            continue
        h, s = line.split("|", 1)
        commits.append((h.strip(), s.strip()))
    return commits


def generate_markdown(
    commits: list[tuple[str, str]],
    prev_tag: str | None = None,
    current_tag: str | None = None,
    repo: str | None = None,
) -> str:
    """Format categorized commits into markdown."""
    if not commits:
        body = "* Maintenance release with internal updates and fixes."
        if prev_tag and current_tag and repo:
            body += f"\n\n**Full Changelog**: https://github.com/{repo}/compare/{prev_tag}...{current_tag}"
        return body

    categories = {
        "feat": ("### 🚀 Features", []),
        "fix": ("### 🐛 Bug Fixes", []),
        "doc": ("### 📝 Documentation", []),
        "chore": ("### 🧰 Chores & Maintenance", []),
    }

    for h, subject in commits:
        cat = categorize_commit(subject)
        categories[cat][1].append((h, subject))

    sections = []
    for _cat_key, (header, items) in categories.items():
        if items:
            lines = [header]
            for h, subject in items:
                if repo:
                    lines.append(f"- [{h}](https://github.com/{repo}/commit/{h}) {subject}")
                else:
                    lines.append(f"- {h} {subject}")
            sections.append("\n".join(lines))

    output = "\n\n".join(sections)

    if prev_tag and current_tag and repo:
        compare_link = f"\n\n**Full Changelog**: https://github.com/{repo}/compare/{prev_tag}...{current_tag}"
        output += compare_link

    return output.strip()


def main():
    prev_tag = sys.argv[1].strip() if len(sys.argv) > 1 and sys.argv[1].strip() else None
    current_tag = sys.argv[2].strip() if len(sys.argv) > 2 and sys.argv[2].strip() else "HEAD"

    # If prev_tag not specified, try to find latest git tag before current_tag
    if not prev_tag:
        try:
            prev_tag = subprocess.check_output(
                ["git", "describe", "--tags", "--abbrev=0", f"{current_tag}^"],
                stderr=subprocess.DEVNULL,
            ).decode("utf-8").strip()
        except Exception:
            prev_tag = None

    commits = get_commits(prev_tag, current_tag)
    repo = get_git_repo()
    markdown = generate_markdown(commits, prev_tag, current_tag, repo)
    print(markdown)


if __name__ == "__main__":
    main()
