import unittest
import sys
import os

# Add scripts directory to path
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

from generate_changelog import generate_markdown


class TestGenerateChangelog(unittest.TestCase):
    def test_generate_markdown_formatting(self):
        commits = [
            ("1234567", "feat: new feature"),
            ("abcdef0", "resolved crash on suspend"),
            ("9876543", "cleaned up build"),
        ]
        md = generate_markdown(
            commits=commits,
            prev_tag="v0.1.0",
            current_tag="v0.1.1",
            repo="nNoidea/niri-bar",
        )
        expected = "\n".join(
            [
                "### What's Changed",
                "- [1234567](https://github.com/nNoidea/niri-bar/commit/1234567) feat: new feature",
                "- [abcdef0](https://github.com/nNoidea/niri-bar/commit/abcdef0) resolved crash on suspend",
                "- [9876543](https://github.com/nNoidea/niri-bar/commit/9876543) cleaned up build",
                "",
                "**Full Changelog**: https://github.com/nNoidea/niri-bar/compare/v0.1.0...v0.1.1",
            ]
        )
        self.assertEqual(md, expected)

    def test_generate_markdown_without_repo_and_empty_commits(self):
        md_plain = generate_markdown(commits=[("1234567", "just a commit")])
        self.assertEqual(md_plain, "### What's Changed\n- 1234567 just a commit")

        md_empty = generate_markdown(
            commits=[],
            prev_tag="v0.1.0",
            current_tag="v0.1.1",
            repo="nNoidea/niri-bar",
        )
        self.assertEqual(
            md_empty,
            "* Maintenance release with internal updates and fixes.\n\n**Full Changelog**: https://github.com/nNoidea/niri-bar/compare/v0.1.0...v0.1.1",
        )


if __name__ == "__main__":
    unittest.main()
