#!/usr/bin/env python3
"""Validate and update Cirrusync's release version.

Cargo.toml is the canonical version source. Cargo.lock must contain exactly one
matching Cirrusync package entry with the same version.
"""

from __future__ import annotations

import argparse
import os
import re
import stat
import subprocess
import sys
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence


PACKAGE_NAME = "cirrusync"
MANIFEST_PATH = Path("Cargo.toml")
LOCKFILE_PATH = Path("Cargo.lock")
REPOSITORY_ROOT = Path(__file__).resolve().parents[1]
MAX_COMPONENT = 999_999_999
MAX_MANIFEST_BYTES = 1_048_576
MAX_LOCKFILE_BYTES = 16_777_216

_COMPONENT_PATTERN = r"(?:0|[1-9][0-9]{0,8})"
_VERSION_RE = re.compile(
    rf"(?P<major>{_COMPONENT_PATTERN})"
    rf"\.(?P<minor>{_COMPONENT_PATTERN})"
    rf"\.(?P<patch>{_COMPONENT_PATTERN})",
    re.ASCII,
)
_MANIFEST_SECTION_RE = re.compile(
    r"^[ \t]*\[(?P<section>[^\[\]\r\n]+)\][ \t]*(?:#.*)?$"
)
_LOCK_TABLE_RE = re.compile(
    r"^[ \t]*\[\[(?P<table>[^\[\]\r\n]+)\]\][ \t]*(?:#.*)?$"
)
_HEX_OBJECT_RE = re.compile(r"[0-9a-fA-F]{40,64}", re.ASCII)


class VersionPolicyError(RuntimeError):
    """A user-actionable version policy failure."""


@dataclass(frozen=True, order=True)
class Version:
    """A strict three-component release version."""

    major: int
    minor: int
    patch: int

    @classmethod
    def parse(cls, value: str, *, source: str) -> "Version":
        match = _VERSION_RE.fullmatch(value)
        if match is None:
            raise VersionPolicyError(
                f"{source} version is invalid; expected X.Y.Z with "
                "no leading zeros and components from 0 through 999999999"
            )

        components = tuple(
            int(match.group(name)) for name in ("major", "minor", "patch")
        )
        if any(component > MAX_COMPONENT for component in components):
            raise VersionPolicyError(
                f"{source} version exceeds the maximum component "
                f"value {MAX_COMPONENT}"
            )
        return cls(*components)

    def bump(self, component: str) -> "Version":
        if component == "patch":
            if self.patch == MAX_COMPONENT:
                raise VersionPolicyError("patch version cannot be incremented further")
            return Version(self.major, self.minor, self.patch + 1)
        if component == "minor":
            if self.minor == MAX_COMPONENT:
                raise VersionPolicyError("minor version cannot be incremented further")
            return Version(self.major, self.minor + 1, 0)
        if component == "major":
            if self.major == MAX_COMPONENT:
                raise VersionPolicyError("major version cannot be incremented further")
            return Version(self.major + 1, 0, 0)
        raise VersionPolicyError(f"unknown version component {component!r}")

    def __str__(self) -> str:
        return f"{self.major}.{self.minor}.{self.patch}"


@dataclass(frozen=True)
class _Assignment:
    line_index: int
    value: str
    prefix: str
    suffix: str
    newline: str


def _split_line_ending(line: str) -> tuple[str, str]:
    if line.endswith("\r\n"):
        return line[:-2], "\r\n"
    if line.endswith("\n") or line.endswith("\r"):
        return line[:-1], line[-1]
    return line, ""


def _match_string_assignment(
    line: str, key: str, line_index: int
) -> _Assignment | None:
    body, newline = _split_line_ending(line)
    match = re.fullmatch(
        rf'(?P<prefix>[ \t]*{re.escape(key)}[ \t]*=[ \t]*")'
        r'(?P<value>[^"\\\r\n]*)'
        r'(?P<suffix>"[ \t]*(?:#.*)?)',
        body,
    )
    if match is None:
        return None
    return _Assignment(
        line_index=line_index,
        value=match.group("value"),
        prefix=match.group("prefix"),
        suffix=match.group("suffix"),
        newline=newline,
    )


def _manifest_package_assignment(
    contents: str, key: str, *, source: str
) -> _Assignment:
    in_package = False
    package_section_count = 0
    matches: list[_Assignment] = []

    for line_index, line in enumerate(contents.splitlines(keepends=True)):
        body, _ = _split_line_ending(line)
        section = _MANIFEST_SECTION_RE.fullmatch(body)
        if section is not None:
            in_package = section.group("section").strip() == "package"
            if in_package:
                package_section_count += 1
            continue
        if body.lstrip().startswith("["):
            in_package = False
            continue
        if in_package:
            assignment = _match_string_assignment(line, key, line_index)
            if assignment is not None:
                matches.append(assignment)

    if package_section_count != 1:
        raise VersionPolicyError(
            f"{source} has {package_section_count} [package] tables; "
            "expected exactly one"
        )
    if not matches:
        raise VersionPolicyError(
            f"{source} has no string {key} in [package]"
        )
    if len(matches) != 1:
        raise VersionPolicyError(
            f"{source} has {len(matches)} package {key} assignments; "
            "expected exactly one"
        )
    return matches[0]


def _manifest_version_assignment(contents: str, *, source: str) -> _Assignment:
    return _manifest_package_assignment(
        contents, "version", source=source
    )


def _lock_version_assignment(
    contents: str,
    *,
    source: str,
    package_index: int | None = None,
    package_count: int | None = None,
) -> _Assignment:
    if package_index is None or package_count is None:
        lock_data = _parse_toml(contents, source=source)
        lock_packages, package_index, _ = _semantic_lock_package(
            lock_data, source=source
        )
        package_count = len(lock_packages)

    package_blocks: list[
        tuple[list[_Assignment], list[_Assignment], list[_Assignment]]
    ] = []
    names: list[_Assignment] | None = None
    versions: list[_Assignment] | None = None
    sources: list[_Assignment] | None = None

    def finish_block() -> None:
        nonlocal names, versions, sources
        if names is not None and versions is not None and sources is not None:
            package_blocks.append((names, versions, sources))
        names = None
        versions = None
        sources = None

    for line_index, line in enumerate(contents.splitlines(keepends=True)):
        body, _ = _split_line_ending(line)
        table = _LOCK_TABLE_RE.fullmatch(body)
        if table is not None:
            finish_block()
            if table.group("table").strip() == "package":
                names = []
                versions = []
                sources = []
            continue
        if body.lstrip().startswith("["):
            finish_block()
            continue
        if names is None or versions is None or sources is None:
            continue

        name = _match_string_assignment(line, "name", line_index)
        if name is not None:
            names.append(name)
        version = _match_string_assignment(line, "version", line_index)
        if version is not None:
            versions.append(version)
        package_source = _match_string_assignment(line, "source", line_index)
        if package_source is not None:
            sources.append(package_source)

    finish_block()

    if len(package_blocks) != package_count:
        raise VersionPolicyError(
            f"{source} has {len(package_blocks)} canonical [[package]] "
            f"blocks but parses as {package_count}; expected an exact match"
        )
    if package_index < 0 or package_index >= len(package_blocks):
        raise VersionPolicyError(
            f"{source} source-less package index is outside the canonical "
            "[[package]] blocks"
        )

    matching_names, matching_versions, _matching_sources = package_blocks[
        package_index
    ]
    if len(matching_names) != 1:
        raise VersionPolicyError(
            f"{source} package {PACKAGE_NAME!r} has {len(matching_names)} "
            "canonical name assignments; expected exactly one"
        )
    if matching_names[0].value != PACKAGE_NAME:
        raise VersionPolicyError(
            f"{source} canonical package name does not match "
            f"{PACKAGE_NAME!r}"
        )
    if len(matching_versions) != 1:
        raise VersionPolicyError(
            f"{source} package {PACKAGE_NAME!r} has {len(matching_versions)} "
            "canonical version assignments; expected exactly one"
        )
    if not _assignment_targets_lock_field(
        contents,
        matching_names[0],
        "name",
        package_index,
        package_count,
        source=source,
    ):
        raise VersionPolicyError(
            f"{source} canonical name assignment does not own the parsed "
            "package field"
        )
    if not _assignment_targets_lock_field(
        contents,
        matching_versions[0],
        "version",
        package_index,
        package_count,
        source=source,
    ):
        raise VersionPolicyError(
            f"{source} canonical version assignment does not own the parsed "
            "package field"
        )
    return matching_versions[0]


def _parse_toml(contents: str, *, source: str) -> dict[str, object]:
    try:
        parsed = tomllib.loads(contents)
    except (tomllib.TOMLDecodeError, ValueError, RecursionError) as exc:
        # Do not echo parser details: the proposed file is untrusted and error
        # messages must not reflect its contents into Actions or terminal logs.
        raise VersionPolicyError(f"{source} is not valid TOML") from exc
    return parsed


def _parsed_lock_packages(
    lock_data: dict[str, object], *, source: str
) -> list[dict[str, object]]:
    lock_packages_value = lock_data.get("package")
    if not isinstance(lock_packages_value, list) or not all(
        isinstance(package, dict) for package in lock_packages_value
    ):
        raise VersionPolicyError(
            f"{source} has no canonical [[package]] array"
        )
    return lock_packages_value


def _semantic_lock_package(
    lock_data: dict[str, object], *, source: str
) -> tuple[list[dict[str, object]], int, dict[str, object]]:
    lock_packages = _parsed_lock_packages(lock_data, source=source)
    semantic_lock_indices = [
        index
        for index, package in enumerate(lock_packages)
        if package.get("name") == PACKAGE_NAME and "source" not in package
    ]
    if len(semantic_lock_indices) != 1:
        raise VersionPolicyError(
            f"{source} has {len(semantic_lock_indices)} semantic source-less "
            f"[[package]] entries named {PACKAGE_NAME!r}; expected exactly one"
        )
    package_index = semantic_lock_indices[0]
    return lock_packages, package_index, lock_packages[package_index]


def _assignment_targets_manifest_field(
    contents: str,
    assignment: _Assignment,
    key: str,
    *,
    source: str,
) -> bool:
    probe = "__cirrusync_assignment_probe__"
    probed_contents = _replace_assignment(contents, assignment, probe)
    probed_data = _parse_toml(probed_contents, source=source)
    package_data = probed_data.get("package")
    return isinstance(package_data, dict) and package_data.get(key) == probe


def _assignment_targets_lock_field(
    contents: str,
    assignment: _Assignment,
    key: str,
    package_index: int,
    package_count: int,
    *,
    source: str,
) -> bool:
    probe = "__cirrusync_assignment_probe__"
    probed_contents = _replace_assignment(contents, assignment, probe)
    probed_data = _parse_toml(probed_contents, source=source)
    packages = _parsed_lock_packages(probed_data, source=source)
    return (
        len(packages) == package_count
        and 0 <= package_index < len(packages)
        and packages[package_index].get(key) == probe
    )


def _validated_version(
    manifest_contents: str,
    lockfile_contents: str,
    *,
    source: str,
) -> Version:
    manifest_source = f"{source} Cargo.toml"
    lock_source = f"{source} Cargo.lock"
    manifest_data = _parse_toml(manifest_contents, source=manifest_source)
    lock_data = _parse_toml(lockfile_contents, source=lock_source)

    package_data = manifest_data.get("package")
    if not isinstance(package_data, dict):
        raise VersionPolicyError(f"{manifest_source} has no [package] table")
    semantic_name = package_data.get("name")
    semantic_manifest_version = package_data.get("version")
    if not isinstance(semantic_name, str):
        raise VersionPolicyError(
            f"{manifest_source} package name is not a string"
        )
    if not isinstance(semantic_manifest_version, str):
        raise VersionPolicyError(
            f"{manifest_source} package version is not a string"
        )

    lock_packages, lock_package_index, semantic_lock_entry = (
        _semantic_lock_package(lock_data, source=lock_source)
    )
    semantic_lock_version = semantic_lock_entry.get("version")
    if not isinstance(semantic_lock_version, str):
        raise VersionPolicyError(
            f"{lock_source} package version is not a string"
        )

    manifest_name = _manifest_package_assignment(
        manifest_contents, "name", source=manifest_source
    )
    if manifest_name.value != semantic_name:
        raise VersionPolicyError(
            f"{manifest_source} canonical name assignment does not match "
            "the parsed package name"
        )
    if not _assignment_targets_manifest_field(
        manifest_contents,
        manifest_name,
        "name",
        source=manifest_source,
    ):
        raise VersionPolicyError(
            f"{manifest_source} canonical name assignment does not own the "
            "parsed package field"
        )
    if semantic_name != PACKAGE_NAME:
        raise VersionPolicyError(
            f"{manifest_source} package name does not match {PACKAGE_NAME!r}"
        )
    manifest_assignment = _manifest_version_assignment(
        manifest_contents, source=manifest_source
    )
    lock_assignment = _lock_version_assignment(
        lockfile_contents,
        source=lock_source,
        package_index=lock_package_index,
        package_count=len(lock_packages),
    )
    if manifest_assignment.value != semantic_manifest_version:
        raise VersionPolicyError(
            f"{manifest_source} canonical version assignment does not match "
            "the parsed package version"
        )
    if not _assignment_targets_manifest_field(
        manifest_contents,
        manifest_assignment,
        "version",
        source=manifest_source,
    ):
        raise VersionPolicyError(
            f"{manifest_source} canonical version assignment does not own the "
            "parsed package field"
        )
    if lock_assignment.value != semantic_lock_version:
        raise VersionPolicyError(
            f"{lock_source} canonical version assignment does not match "
            "the parsed package version"
        )
    manifest_version = Version.parse(
        semantic_manifest_version, source=manifest_source
    )
    lock_version = Version.parse(
        semantic_lock_version, source=lock_source
    )
    if lock_version != manifest_version:
        raise VersionPolicyError(
            f"{source} Cargo.lock version {lock_version} does not match "
            f"Cargo.toml version {manifest_version}"
        )
    return manifest_version


def _replace_assignment(
    contents: str, assignment: _Assignment, new_value: str
) -> str:
    lines = contents.splitlines(keepends=True)
    lines[assignment.line_index] = (
        f"{assignment.prefix}{new_value}{assignment.suffix}{assignment.newline}"
    )
    return "".join(lines)


def _read_text(path: Path, *, maximum_bytes: int) -> str:
    try:
        if path.is_symlink():
            raise VersionPolicyError(f"{path} must not be a symbolic link")
        metadata = path.stat()
        if not stat.S_ISREG(metadata.st_mode):
            raise VersionPolicyError(f"{path} is not a regular file")
        size = metadata.st_size
        if size > maximum_bytes:
            raise VersionPolicyError(
                f"{path} is too large ({size} bytes; maximum {maximum_bytes})"
            )
        with path.open("r", encoding="utf-8", newline="") as handle:
            return handle.read()
    except (OSError, UnicodeError) as exc:
        raise VersionPolicyError(f"cannot read {path}: {exc}") from exc


def _atomic_write(path: Path, contents: str) -> None:
    descriptor, temporary_name = tempfile.mkstemp(
        prefix=f".{path.name}.", dir=path.parent
    )
    temporary = Path(temporary_name)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8", newline="") as handle:
            handle.write(contents)
            handle.flush()
            os.fsync(handle.fileno())
        os.chmod(temporary, stat.S_IMODE(path.stat().st_mode))
        os.replace(temporary, path)
    finally:
        try:
            temporary.unlink()
        except FileNotFoundError:
            pass


class VersionRepository:
    """Version operations rooted at one Cirrusync checkout."""

    def __init__(self, root: Path) -> None:
        try:
            self.root = root.resolve()
        except (OSError, RuntimeError) as exc:
            raise VersionPolicyError("cannot resolve repository root") from exc
        self.manifest = self.root / MANIFEST_PATH
        self.lockfile = self.root / LOCKFILE_PATH

    def _working_contents(self) -> tuple[str, str]:
        return (
            _read_text(self.manifest, maximum_bytes=MAX_MANIFEST_BYTES),
            _read_text(self.lockfile, maximum_bytes=MAX_LOCKFILE_BYTES),
        )

    def validate(self) -> Version:
        manifest, lockfile = self._working_contents()
        return _validated_version(manifest, lockfile, source="working tree")

    def bump(self, component: str) -> Version:
        original_manifest, original_lockfile = self._working_contents()
        current = _validated_version(
            original_manifest, original_lockfile, source="working tree"
        )
        new_version = current.bump(component)

        manifest_assignment = _manifest_version_assignment(
            original_manifest, source="working tree Cargo.toml"
        )
        lock_assignment = _lock_version_assignment(
            original_lockfile, source="working tree Cargo.lock"
        )
        new_manifest = _replace_assignment(
            original_manifest, manifest_assignment, str(new_version)
        )
        new_lockfile = _replace_assignment(
            original_lockfile, lock_assignment, str(new_version)
        )
        _validated_version(
            new_manifest, new_lockfile, source="updated working tree"
        )

        try:
            _atomic_write(self.manifest, new_manifest)
            _atomic_write(self.lockfile, new_lockfile)
        except OSError as exc:
            rollback_errors = []
            for path, contents in (
                (self.manifest, original_manifest),
                (self.lockfile, original_lockfile),
            ):
                try:
                    _atomic_write(path, contents)
                except OSError as rollback_error:
                    rollback_errors.append(f"{path}: {rollback_error}")
            detail = ""
            if rollback_errors:
                detail = f"; rollback also failed: {', '.join(rollback_errors)}"
            raise VersionPolicyError(
                f"failed to update version files: {exc}{detail}"
            ) from exc

        return new_version

    def _resolve_git_ref(self, ref: str) -> str:
        if not ref or len(ref) > 1024 or ref.startswith("-") or any(
            character.isspace() or ord(character) < 32 for character in ref
        ):
            raise VersionPolicyError("invalid Git ref")
        try:
            result = subprocess.run(
                ["git", "rev-parse", "--verify", "--quiet", f"{ref}^{{commit}}"],
                cwd=self.root,
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
        except OSError as exc:
            raise VersionPolicyError(f"cannot run Git: {exc}") from exc
        try:
            object_id = result.stdout.decode("ascii").strip()
        except UnicodeDecodeError as exc:
            raise VersionPolicyError(
                "Git ref did not resolve to an ASCII object ID"
            ) from exc
        if result.returncode != 0 or _HEX_OBJECT_RE.fullmatch(object_id) is None:
            raise VersionPolicyError("Git ref does not resolve to a commit")
        return object_id.lower()

    def _file_at_commit(self, commit: str, path: Path) -> str:
        object_spec = f"{commit}:{path.as_posix()}"
        maximum_bytes = (
            MAX_MANIFEST_BYTES
            if path == MANIFEST_PATH
            else MAX_LOCKFILE_BYTES
        )
        try:
            tree_result = subprocess.run(
                ["git", "ls-tree", commit, "--", path.as_posix()],
                cwd=self.root,
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
            tree_prefix = tree_result.stdout.split(b"\t", 1)[0].split()
            if (
                tree_result.returncode != 0
                or len(tree_prefix) != 3
                or tree_prefix[0] != b"100644"
                or tree_prefix[1] != b"blob"
                or re.fullmatch(rb"[0-9a-fA-F]{40,64}", tree_prefix[2])
                is None
            ):
                raise VersionPolicyError(
                    f"{path.as_posix()} at the requested Git ref is not a "
                    "regular non-executable file"
                )
            size_result = subprocess.run(
                ["git", "cat-file", "-s", object_spec],
                cwd=self.root,
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
            size_text = size_result.stdout.strip()
            if (
                size_result.returncode != 0
                or re.fullmatch(rb"[0-9]+", size_text) is None
            ):
                raise VersionPolicyError(
                    f"cannot inspect {path.as_posix()} at the requested Git ref"
                )
            size = int(size_text)
            if size > maximum_bytes:
                raise VersionPolicyError(
                    f"{path.as_posix()} at the requested Git ref is too large "
                    f"({size} bytes; maximum {maximum_bytes})"
                )
            result = subprocess.run(
                ["git", "cat-file", "blob", object_spec],
                cwd=self.root,
                check=False,
                stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL,
            )
        except OSError as exc:
            raise VersionPolicyError(f"cannot run Git: {exc}") from exc
        if result.returncode != 0:
            raise VersionPolicyError(
                f"cannot read {path.as_posix()} at the requested Git ref"
            )
        try:
            return result.stdout.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise VersionPolicyError(
                f"{path.as_posix()} at the requested Git ref is not UTF-8"
            ) from exc

    def version_at(self, ref: str) -> Version:
        commit = self._resolve_git_ref(ref)
        manifest = self._file_at_commit(commit, MANIFEST_PATH)
        lockfile = self._file_at_commit(commit, LOCKFILE_PATH)
        return _validated_version(
            manifest, lockfile, source="requested Git commit"
        )

    def check_pr(self, base_ref: str, head_ref: str) -> tuple[Version, Version]:
        base = self.version_at(base_ref)
        head = self.version_at(head_ref)
        if head <= base:
            raise VersionPolicyError(
                f"pull request version must be strictly greater than the base: "
                f"{head} is not greater than {base}"
            )
        return base, head


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--root",
        type=Path,
        default=REPOSITORY_ROOT,
        help="repository root (default: inferred from this script)",
    )
    subparsers = parser.add_subparsers(dest="command", required=True)

    subparsers.add_parser("get", help="print the validated current version")
    subparsers.add_parser(
        "validate", help="validate Cargo.toml and Cargo.lock synchronization"
    )

    check_pr = subparsers.add_parser(
        "check-pr", help="require the head version to be greater than the base"
    )
    check_pr.add_argument("--base", required=True, help="base commit or Git ref")
    check_pr.add_argument("--head", required=True, help="head commit or Git ref")

    bump = subparsers.add_parser(
        "bump", help="increment one version component and synchronize Cargo.lock"
    )
    bump.add_argument("component", choices=("patch", "minor", "major"))
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        repository = VersionRepository(args.root)
        if args.command == "get":
            print(repository.validate())
        elif args.command == "validate":
            version = repository.validate()
            print(f"version {version} is valid and synchronized")
        elif args.command == "check-pr":
            base, head = repository.check_pr(args.base, args.head)
            print(f"version increased from {base} to {head}")
        elif args.command == "bump":
            print(repository.bump(args.component))
        else:
            raise VersionPolicyError(f"unsupported command {args.command!r}")
    except VersionPolicyError as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
