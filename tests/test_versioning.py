"""Tests for the standalone version policy helper."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import shutil
import stat
import subprocess
import sys
import unittest
import uuid
import warnings
from unittest import mock
from pathlib import Path


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
MODULE_PATH = REPOSITORY_ROOT / "scripts" / "versioning.py"
TEST_TEMP_ROOT = REPOSITORY_ROOT / "target" / "versioning-tests"
TEST_TEMP_ROOT.mkdir(parents=True, exist_ok=True)
SPEC = importlib.util.spec_from_file_location("cirrusync_versioning", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise RuntimeError(f"cannot load {MODULE_PATH}")
versioning = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = versioning
SPEC.loader.exec_module(versioning)


def write_version_files(
    root: Path,
    version: str,
    *,
    lock_version: str | None = None,
    newline: str = "\n",
    inline_comment: bool = False,
) -> None:
    comment = " # canonical release" if inline_comment else ""
    manifest = newline.join(
        (
            "[package]",
            'name = "cirrusync"',
            f'version = "{version}"{comment}',
            'edition = "2024"',
            "",
            "[dependencies]",
            'serde = "1"',
            "",
        )
    )
    lockfile = newline.join(
        (
            "version = 4",
            "",
            "[[package]]",
            'name = "cirrusync"',
            f'version = "{lock_version or version}"{comment}',
            "dependencies = [",
            ' \"serde\",',
            "]",
            "",
            "[[package]]",
            'name = "serde"',
            'version = "1.0.0"',
            'source = "registry+https://github.com/rust-lang/crates.io-index"',
            "",
        )
    )
    (root / "Cargo.toml").write_bytes(manifest.encode())
    (root / "Cargo.lock").write_bytes(lockfile.encode())


class TemporaryRepository:
    def __init__(self, version: str = "0.1.0") -> None:
        self.root = TEST_TEMP_ROOT / f"repository-{uuid.uuid4().hex}"
        self.root.mkdir()
        write_version_files(self.root, version)

    def close(self) -> None:
        def remove_readonly(
            function: object, path: str, _error: object
        ) -> None:
            Path(path).chmod(stat.S_IREAD | stat.S_IWRITE | stat.S_IEXEC)
            function(path)  # type: ignore[operator]

        try:
            shutil.rmtree(self.root, onerror=remove_readonly)
        except OSError as exc:
            warnings.warn(
                f"could not remove version-test repository {self.root}: {exc}",
                RuntimeWarning,
                stacklevel=2,
            )

    def __enter__(self) -> "TemporaryRepository":
        return self

    def __exit__(self, *_args: object) -> None:
        self.close()

    def git(self, *args: str) -> str:
        result = subprocess.run(
            ["git", *args],
            cwd=self.root,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        return result.stdout.strip()

    def initialize_git(self) -> str:
        self.git("init", "--quiet")
        self.git("config", "user.name", "Version Tests")
        self.git("config", "user.email", "version-tests@example.invalid")
        self.git("add", "Cargo.toml", "Cargo.lock")
        self.git("commit", "--quiet", "-m", "initial version")
        return self.git("rev-parse", "HEAD")

    def commit(self, message: str) -> str:
        self.git("add", "Cargo.toml", "Cargo.lock")
        self.git("commit", "--quiet", "-m", message)
        return self.git("rev-parse", "HEAD")


class VersionParsingTests(unittest.TestCase):
    def test_accepts_strict_release_boundaries(self) -> None:
        for raw in ("0.0.0", "1.2.3", "999999999.999999999.999999999"):
            with self.subTest(raw=raw):
                self.assertEqual(str(versioning.Version.parse(raw, source="test")), raw)

    def test_rejects_non_release_or_noncanonical_versions(self) -> None:
        invalid = (
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.02.3",
            "1.2.03",
            "1.2.3-alpha.1",
            "1.2.3+build",
            "1000000000.0.0",
            "0.1000000000.0",
            "0.0.1000000000",
            "١.٢.٣",
            " 1.2.3",
        )
        for raw in invalid:
            with self.subTest(raw=raw):
                with self.assertRaises(versioning.VersionPolicyError):
                    versioning.Version.parse(raw, source="test")

    def test_bump_resets_lower_components(self) -> None:
        current = versioning.Version(1, 2, 3)
        self.assertEqual(current.bump("patch"), versioning.Version(1, 2, 4))
        self.assertEqual(current.bump("minor"), versioning.Version(1, 3, 0))
        self.assertEqual(current.bump("major"), versioning.Version(2, 0, 0))

    def test_bump_rejects_component_overflow(self) -> None:
        cases = (
            (versioning.Version(1, 2, versioning.MAX_COMPONENT), "patch"),
            (versioning.Version(1, versioning.MAX_COMPONENT, 3), "minor"),
            (versioning.Version(versioning.MAX_COMPONENT, 2, 3), "major"),
        )
        for current, component in cases:
            with self.subTest(current=current, component=component):
                with self.assertRaises(versioning.VersionPolicyError):
                    current.bump(component)


class RepositoryValidationTests(unittest.TestCase):
    def test_validate_and_get_require_synchronized_files(self) -> None:
        with TemporaryRepository("1.2.3") as temporary:
            repository = versioning.VersionRepository(temporary.root)
            self.assertEqual(repository.validate(), versioning.Version(1, 2, 3))

            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                status = versioning.main(
                    ["--root", str(temporary.root), "get"]
                )
            self.assertEqual(status, 0)
            self.assertEqual(output.getvalue(), "1.2.3\n")

    def test_validate_rejects_lockfile_mismatch(self) -> None:
        with TemporaryRepository() as temporary:
            write_version_files(
                temporary.root, "1.2.3", lock_version="1.2.2"
            )
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "does not match"
            ):
                versioning.VersionRepository(temporary.root).validate()

    def test_validate_rejects_duplicate_cirrusync_lock_entries(self) -> None:
        with TemporaryRepository("1.2.3") as temporary:
            lockfile = temporary.root / "Cargo.lock"
            with lockfile.open("a", encoding="utf-8", newline="") as handle:
                handle.write(
                    '\n[[package]]\nname = "cirrusync"\nversion = "1.2.3"\n'
                )
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "expected exactly one"
            ):
                versioning.VersionRepository(temporary.root).validate()

    def test_validate_selects_the_source_less_root_lock_entry(self) -> None:
        with TemporaryRepository("1.2.3") as temporary:
            lockfile = temporary.root / "Cargo.lock"
            with lockfile.open("a", encoding="utf-8") as handle:
                handle.write(
                    "\n[[package]]\n"
                    'name = "cirrusync"\n'
                    'version = "9.9.9"\n'
                    'source = "registry+https://example.invalid/index"\n'
                )
            self.assertEqual(
                versioning.VersionRepository(temporary.root).validate(),
                versioning.Version(1, 2, 3),
            )

            contents = lockfile.read_text(encoding="utf-8")
            lockfile.write_text(
                contents.replace(
                    'name = "cirrusync"\nversion = "1.2.3"',
                    'name = "cirrusync"\n'
                    'version = "1.2.3"\n'
                    'source = "registry+https://example.invalid/root"',
                    1,
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "source-less"
            ):
                versioning.VersionRepository(temporary.root).validate()

    def test_sourced_decoy_cannot_stand_in_for_canonical_root(self) -> None:
        with TemporaryRepository("1.2.3") as temporary:
            lockfile = temporary.root / "Cargo.lock"
            lockfile.write_text(
                "version = 4\n\n"
                "[[package]]\n"
                "name = 'cirrusync'\n"
                "version = '1.2.3'\n\n"
                "[[package]]\n"
                'name = "cirrusync"\n'
                'version = "1.2.3"\n'
                "source = 'registry+https://example.invalid/index'\n",
                encoding="utf-8",
            )
            repository = versioning.VersionRepository(temporary.root)
            before = lockfile.read_bytes()

            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "canonical name assignments"
            ):
                repository.validate()
            with self.assertRaises(versioning.VersionPolicyError):
                repository.bump("patch")
            self.assertEqual(lockfile.read_bytes(), before)

    def test_validate_rejects_duplicate_name_in_matching_lock_entry(self) -> None:
        with TemporaryRepository("1.2.3") as temporary:
            lockfile = temporary.root / "Cargo.lock"
            contents = lockfile.read_text(encoding="utf-8")
            lockfile.write_text(
                contents.replace(
                    'name = "cirrusync"\nversion = "1.2.3"',
                    'name = "cirrusync"\nname = "other"\nversion = "1.2.3"',
                    1,
                ),
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "not valid TOML"
            ):
                versioning.VersionRepository(temporary.root).validate()

    def test_validate_rejects_missing_manifest_version(self) -> None:
        with TemporaryRepository() as temporary:
            (temporary.root / "Cargo.toml").write_text(
                '[package]\nname = "cirrusync"\n',
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "package version is not a string"
            ):
                versioning.VersionRepository(temporary.root).validate()

    def test_validate_rejects_assignments_inside_multiline_strings(self) -> None:
        for delimiter in ('"""', "'''"):
            with self.subTest(file="Cargo.toml", delimiter=delimiter):
                with TemporaryRepository("1.2.3") as temporary:
                    (temporary.root / "Cargo.toml").write_text(
                        "[package]\n"
                        'name = "cirrusync"\n'
                        f"description = {delimiter}\n"
                        'version = "1.2.3"\n'
                        f"{delimiter}\n",
                        encoding="utf-8",
                    )
                    with self.assertRaises(versioning.VersionPolicyError):
                        versioning.VersionRepository(
                            temporary.root
                        ).validate()

            with self.subTest(file="Cargo.lock", delimiter=delimiter):
                with TemporaryRepository("1.2.3") as temporary:
                    (temporary.root / "Cargo.lock").write_text(
                        "version = 4\n\n"
                        "[[package]]\n"
                        f"description = {delimiter}\n"
                        'name = "cirrusync"\n'
                        'version = "1.2.3"\n'
                        f"{delimiter}\n",
                        encoding="utf-8",
                    )
                    with self.assertRaises(versioning.VersionPolicyError):
                        versioning.VersionRepository(
                            temporary.root
                        ).validate()

    def test_multiline_decoys_cannot_impersonate_semantic_fields(self) -> None:
        with self.subTest(file="Cargo.toml"):
            with TemporaryRepository("1.2.3") as temporary:
                manifest = temporary.root / "Cargo.toml"
                manifest.write_text(
                    "[package]\n"
                    '"name" = "cirrusync"\n'
                    '"version" = "1.2.3"\n'
                    'description = """\n'
                    'name = "cirrusync"\n'
                    'version = "1.2.3"\n'
                    '"""\n',
                    encoding="utf-8",
                )
                repository = versioning.VersionRepository(temporary.root)
                before = manifest.read_bytes()

                with self.assertRaisesRegex(
                    versioning.VersionPolicyError, "does not own"
                ):
                    repository.validate()
                with self.assertRaises(versioning.VersionPolicyError):
                    repository.bump("patch")
                self.assertEqual(manifest.read_bytes(), before)

        with self.subTest(file="Cargo.lock"):
            with TemporaryRepository("1.2.3") as temporary:
                lockfile = temporary.root / "Cargo.lock"
                lockfile.write_text(
                    "version = 4\n\n"
                    "[[package]]\n"
                    '"name" = "cirrusync"\n'
                    '"version" = "1.2.3"\n'
                    'description = """\n'
                    'name = "cirrusync"\n'
                    'version = "1.2.3"\n'
                    '"""\n',
                    encoding="utf-8",
                )
                repository = versioning.VersionRepository(temporary.root)
                before = lockfile.read_bytes()

                with self.assertRaisesRegex(
                    versioning.VersionPolicyError, "does not own"
                ):
                    repository.validate()
                with self.assertRaises(versioning.VersionPolicyError):
                    repository.bump("patch")
                self.assertEqual(lockfile.read_bytes(), before)

    def test_validate_rejects_assignments_inside_multiline_arrays(self) -> None:
        with TemporaryRepository("1.2.3") as temporary:
            (temporary.root / "Cargo.toml").write_text(
                "[package]\n"
                'name = "cirrusync"\n'
                "keywords = [\n"
                'version = "1.2.3"\n'
                "]\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "not valid TOML"
            ):
                versioning.VersionRepository(temporary.root).validate()

        with TemporaryRepository("1.2.3") as temporary:
            (temporary.root / "Cargo.lock").write_text(
                "version = 4\n\n"
                "[[package]]\n"
                "dependencies = [\n"
                'name = "cirrusync"\n'
                'version = "1.2.3"\n'
                "]\n",
                encoding="utf-8",
            )
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "not valid TOML"
            ):
                versioning.VersionRepository(temporary.root).validate()

    def test_validate_rejects_duplicate_package_tables(self) -> None:
        with TemporaryRepository("1.2.3") as temporary:
            manifest = temporary.root / "Cargo.toml"
            with manifest.open("a", encoding="utf-8") as handle:
                handle.write('\n[package]\nversion = "1.2.3"\n')
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "not valid TOML"
            ):
                versioning.VersionRepository(temporary.root).validate()

    def test_validate_rejects_invalid_utf8_without_traceback(self) -> None:
        with TemporaryRepository("1.2.3") as temporary:
            (temporary.root / "Cargo.toml").write_bytes(b"\xff")
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "cannot read"
            ):
                versioning.VersionRepository(temporary.root).validate()

    def test_validate_rejects_oversized_manifest(self) -> None:
        with TemporaryRepository("1.2.3") as temporary:
            (temporary.root / "Cargo.toml").write_bytes(
                b"x" * (versioning.MAX_MANIFEST_BYTES + 1)
            )
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "too large"
            ):
                versioning.VersionRepository(temporary.root).validate()

    def test_validate_rejects_wrong_or_duplicate_manifest_name(self) -> None:
        for replacement in (
            'name = "other"',
            'name = "cirrusync"\nname = "other"',
        ):
            with self.subTest(replacement=replacement):
                with TemporaryRepository("1.2.3") as temporary:
                    manifest = temporary.root / "Cargo.toml"
                    contents = manifest.read_text(encoding="utf-8")
                    manifest.write_text(
                        contents.replace(
                            'name = "cirrusync"', replacement, 1
                        ),
                        encoding="utf-8",
                    )
                    with self.assertRaises(versioning.VersionPolicyError):
                        versioning.VersionRepository(
                            temporary.root
                        ).validate()

    def test_validate_command_reports_failure_without_traceback(self) -> None:
        with TemporaryRepository() as temporary:
            write_version_files(
                temporary.root, "1.2.3", lock_version="1.2.2"
            )
            errors = io.StringIO()
            with contextlib.redirect_stderr(errors):
                status = versioning.main(
                    ["--root", str(temporary.root), "validate"]
                )
            self.assertEqual(status, 1)
            self.assertIn("error:", errors.getvalue())
            self.assertNotIn("Traceback", errors.getvalue())

    def test_pathological_toml_errors_do_not_escape_main(self) -> None:
        cases = (
            ("Cargo.toml", "value = " + ("9" * 5000)),
            ("Cargo.lock", "value = " + ("[" * 1500) + "0" + ("]" * 1500)),
        )
        for filename, contents in cases:
            with self.subTest(filename=filename):
                with TemporaryRepository() as temporary:
                    (temporary.root / filename).write_text(
                        contents, encoding="utf-8"
                    )
                    errors = io.StringIO()
                    with contextlib.redirect_stderr(errors):
                        status = versioning.main(
                            ["--root", str(temporary.root), "validate"]
                        )
                    self.assertEqual(status, 1)
                    self.assertIn("error:", errors.getvalue())
                    self.assertNotIn("Traceback", errors.getvalue())


class BumpTests(unittest.TestCase):
    def test_bump_updates_manifest_and_lockfile(self) -> None:
        cases = (
            ("patch", "1.2.4"),
            ("minor", "1.3.0"),
            ("major", "2.0.0"),
        )
        for component, expected in cases:
            with self.subTest(component=component):
                with TemporaryRepository("1.2.3") as temporary:
                    repository = versioning.VersionRepository(temporary.root)
                    self.assertEqual(str(repository.bump(component)), expected)
                    self.assertEqual(str(repository.validate()), expected)
                    self.assertIn(
                        f'version = "{expected}"',
                        (temporary.root / "Cargo.toml").read_text(
                            encoding="utf-8"
                        ),
                    )
                    self.assertIn(
                        f'name = "cirrusync"\nversion = "{expected}"',
                        (temporary.root / "Cargo.lock").read_text(
                            encoding="utf-8"
                        ),
                    )

    def test_bump_preserves_crlf_and_inline_comments(self) -> None:
        with TemporaryRepository() as temporary:
            write_version_files(
                temporary.root,
                "1.2.3",
                newline="\r\n",
                inline_comment=True,
            )
            versioning.VersionRepository(temporary.root).bump("patch")

            manifest = (temporary.root / "Cargo.toml").read_bytes()
            lockfile = (temporary.root / "Cargo.lock").read_bytes()
            self.assertIn(
                b'version = "1.2.4" # canonical release\r\n', manifest
            )
            self.assertIn(
                b'version = "1.2.4" # canonical release\r\n', lockfile
            )
            self.assertNotIn(b"\n", manifest.replace(b"\r\n", b""))
            self.assertNotIn(b"\n", lockfile.replace(b"\r\n", b""))

    def test_overflow_does_not_modify_either_file(self) -> None:
        with TemporaryRepository("1.2.999999999") as temporary:
            manifest = temporary.root / "Cargo.toml"
            lockfile = temporary.root / "Cargo.lock"
            before = (manifest.read_bytes(), lockfile.read_bytes())

            with self.assertRaises(versioning.VersionPolicyError):
                versioning.VersionRepository(temporary.root).bump("patch")

            self.assertEqual(
                (manifest.read_bytes(), lockfile.read_bytes()),
                before,
            )

    def test_second_replace_failure_restores_both_files(self) -> None:
        with TemporaryRepository("1.2.3") as temporary:
            repository = versioning.VersionRepository(temporary.root)
            manifest = temporary.root / "Cargo.toml"
            lockfile = temporary.root / "Cargo.lock"
            before = (manifest.read_bytes(), lockfile.read_bytes())
            real_atomic_write = versioning._atomic_write

            def fail_updated_lock(path: Path, contents: str) -> None:
                if (
                    path == lockfile
                    and 'version = "1.2.4"' in contents
                ):
                    raise OSError("injected lockfile replacement failure")
                real_atomic_write(path, contents)

            with mock.patch.object(
                versioning, "_atomic_write", side_effect=fail_updated_lock
            ):
                with self.assertRaisesRegex(
                    versioning.VersionPolicyError,
                    "failed to update version files",
                ):
                    repository.bump("patch")

            self.assertEqual(
                (manifest.read_bytes(), lockfile.read_bytes()),
                before,
            )


class PullRequestPolicyTests(unittest.TestCase):
    def test_check_pr_requires_strictly_greater_head(self) -> None:
        with TemporaryRepository("0.1.0") as temporary:
            base = temporary.initialize_git()
            repository = versioning.VersionRepository(temporary.root)
            repository.bump("patch")
            head = temporary.commit("bump version")

            self.assertEqual(
                repository.check_pr(base, head),
                (versioning.Version(0, 1, 0), versioning.Version(0, 1, 1)),
            )

            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "strictly greater"
            ):
                repository.check_pr(base, base)
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "strictly greater"
            ):
                repository.check_pr(head, base)

    def test_check_pr_validates_lockfile_at_each_ref(self) -> None:
        with TemporaryRepository("0.1.0") as temporary:
            base = temporary.initialize_git()
            write_version_files(
                temporary.root, "0.1.1", lock_version="0.1.0"
            )
            head = temporary.commit("inconsistent version")

            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "does not match"
            ):
                versioning.VersionRepository(temporary.root).check_pr(
                    base, head
                )

    def test_check_pr_rejects_invalid_refs(self) -> None:
        with TemporaryRepository() as temporary:
            temporary.initialize_git()
            repository = versioning.VersionRepository(temporary.root)
            for ref in ("", "--help", "HEAD\nother", "does-not-exist"):
                with self.subTest(ref=ref):
                    with self.assertRaises(versioning.VersionPolicyError):
                        repository.version_at(ref)

    def test_check_pr_rejects_invalid_utf8_blob_without_traceback(self) -> None:
        with TemporaryRepository("0.1.0") as temporary:
            base = temporary.initialize_git()
            (temporary.root / "Cargo.toml").write_bytes(b"\xff")
            head = temporary.commit("invalid UTF-8 manifest")
            with self.assertRaisesRegex(
                versioning.VersionPolicyError, "not UTF-8"
            ):
                versioning.VersionRepository(temporary.root).check_pr(
                    base, head
                )

    def test_git_versions_require_regular_non_executable_files(self) -> None:
        with TemporaryRepository("0.1.0") as temporary:
            temporary.initialize_git()
            temporary.git(
                "update-index", "--chmod=+x", "Cargo.toml"
            )
            temporary.git(
                "commit", "--quiet", "-m", "make manifest executable"
            )
            head = temporary.git("rev-parse", "HEAD")
            with self.assertRaisesRegex(
                versioning.VersionPolicyError,
                "regular non-executable file",
            ):
                versioning.VersionRepository(temporary.root).version_at(head)


if __name__ == "__main__":
    unittest.main()
