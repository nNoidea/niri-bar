import unittest
import sys
import os

# Add scripts directory to path
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from generate_changelog import categorize_commit, generate_markdown


class TestGenerateChangelog(unittest.TestCase):
    def test_categorize_bug_fixes(self):
        cases = [
            "fix ci error",
            "fix(volume): click behavior",
            "Fixed wpctl missing output",
            "resolved nasty bug in tray",
            "hotfix: prevent crash on reload",
        ]
        for msg in cases:
            with self.subTest(msg=msg):
                self.assertEqual(categorize_commit(msg), "fix")

    def test_categorize_features(self):
        cases = [
            "feat: add brightness slider",
            "feat(ci): auto-detect release",
            "Add battery indicator",
            "added new workspace preview",
            "implemented custom fonts",
        ]
        for msg in cases:
            with self.subTest(msg=msg):
                self.assertEqual(categorize_commit(msg), "feat")

    def test_categorize_documentation(self):
        cases = [
            "docs: update readme with install steps",
            "update README.md",
            "documentation for configuration",
        ]
        for msg in cases:
            with self.subTest(msg=msg):
                self.assertEqual(categorize_commit(msg), "doc")

    def test_categorize_fallback_chore(self):
        cases = [
            "chore: remove pre-push hook",
            "test error message with multiple lines",
            "random commit without keywords",
            "0.1.0",
            "cleanup unused variables",
        ]
        for msg in cases:
            with self.subTest(msg=msg):
                self.assertEqual(categorize_commit(msg), "chore")

    def test_generate_markdown_formatting(self):
        commits = [
            ("1234567", "feat: new feature"),
            ("abcdef0", "fix: resolved crash"),
            ("9876543", "chore: cleaned up build"),
        ]
        md = generate_markdown(
            commits=commits,
            prev_tag="v0.1.0",
            current_tag="v0.1.1",
            repo="nNoidea/niri-bar",
        )
        self.assertIn("### 🚀 Features", md)
        self.assertIn("[1234567](https://github.com/nNoidea/niri-bar/commit/1234567) feat: new feature", md)
        self.assertIn("### 🐛 Bug Fixes", md)
        self.assertIn("[abcdef0](https://github.com/nNoidea/niri-bar/commit/abcdef0) fix: resolved crash", md)
        self.assertIn("### 🧰 Chores & Maintenance", md)
        self.assertIn("[9876543](https://github.com/nNoidea/niri-bar/commit/9876543) chore: cleaned up build", md)
        self.assertIn("**Full Changelog**: https://github.com/nNoidea/niri-bar/compare/v0.1.0...v0.1.1", md)


if __name__ == "__main__":
    unittest.main()
