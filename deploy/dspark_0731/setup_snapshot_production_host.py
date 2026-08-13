#!/usr/bin/env python3
"""Create the fixed host authority for the production snapshot companions.

The production entry point intentionally has no path or identity overrides.  Unit
tests exercise the policy through an in-memory backend; operators can therefore
test the dangerous decisions without creating host users or writing under /run.
"""

from __future__ import annotations

import argparse
import dataclasses
import grp
import os
import pathlib
import pwd
import stat
import subprocess
import sys
from collections.abc import Sequence


class SetupError(RuntimeError):
    """A closed, non-secret-bearing setup failure."""


@dataclasses.dataclass(frozen=True)
class GroupContract:
    name: str
    gid: int


@dataclasses.dataclass(frozen=True)
class UserContract:
    name: str
    uid: int
    primary_gid: int
    supplementary_gids: frozenset[int]


@dataclasses.dataclass(frozen=True)
class GroupRecord:
    name: str
    gid: int


@dataclasses.dataclass(frozen=True)
class UserRecord:
    name: str
    uid: int
    primary_gid: int
    home: str
    shell: str
    supplementary_gids: frozenset[int]


@dataclasses.dataclass(frozen=True)
class FileState:
    kind: str
    uid: int
    gid: int
    mode: int
    nlink: int
    size: int
    device: int
    inode: int
    filesystem: str
    safe_path: bool = True


@dataclasses.dataclass(frozen=True)
class DirectoryContract:
    path: str
    uid: int
    gid: int
    mode: int


SESSION_GROUP = GroupContract("mini-dynamo-snapshot", 12000)
METRICS_A_GROUP = GroupContract("mini-dynamo-snapshot-metrics-a", 12004)
METRICS_B_GROUP = GroupContract("mini-dynamo-snapshot-metrics-b", 12005)
GROUPS = (SESSION_GROUP, METRICS_A_GROUP, METRICS_B_GROUP)

USERS = (
    UserContract("mini-dynamo-snapshot-a", 12001, 12000, frozenset({12004})),
    UserContract("mini-dynamo", 12002, 12000, frozenset()),
    UserContract("mini-dynamo-snapshot-b", 12003, 12000, frozenset({12005})),
)

DIRECTORIES = (
    DirectoryContract("/run/mini-dynamo-snapshot-a", 12001, 12000, 0o2750),
    DirectoryContract("/run/mini-dynamo-snapshot-b", 12003, 12000, 0o2750),
    DirectoryContract("/run/mini-dynamo-snapshot-metrics-a", 12001, 12004, 0o2750),
    DirectoryContract("/run/mini-dynamo-snapshot-metrics-b", 12003, 12005, 0o2750),
    DirectoryContract("/run/mini-dynamo-snapshot-attestation-a", 0, 12000, 0o2750),
    DirectoryContract("/run/mini-dynamo-snapshot-attestation-b", 0, 12000, 0o2750),
)

SECRETS = (
    "/run/secrets/mini-dynamo-snapshot-session-a",
    "/run/secrets/mini-dynamo-snapshot-session-b",
    "/run/secrets/mini-dynamo-snapshot-digest-a",
    "/run/secrets/mini-dynamo-snapshot-digest-b",
)

METADATA_TARGETS = (
    "/run/mini-dynamo-engine-metadata-a.json",
    "/run/mini-dynamo-engine-metadata-b.json",
)

ATTESTATION_TARGETS = (
    "/run/mini-dynamo-snapshot-attestation-a/engine.json",
    "/run/mini-dynamo-snapshot-attestation-b/engine.json",
)

NONLOGIN_SHELLS = frozenset({"/usr/sbin/nologin", "/sbin/nologin", "/bin/false"})


class HostBackend:
    """Minimal mutation surface used by the fail-closed policy."""

    def assert_privileged(self) -> None:
        raise NotImplementedError

    def group_by_name(self, name: str) -> GroupRecord | None:
        raise NotImplementedError

    def group_by_gid(self, gid: int) -> GroupRecord | None:
        raise NotImplementedError

    def create_group(self, contract: GroupContract) -> None:
        raise NotImplementedError

    def user_by_name(self, name: str) -> UserRecord | None:
        raise NotImplementedError

    def user_by_uid(self, uid: int) -> UserRecord | None:
        raise NotImplementedError

    def create_user(self, contract: UserContract) -> None:
        raise NotImplementedError

    def add_supplementary_groups(self, user: str, gids: frozenset[int]) -> None:
        raise NotImplementedError

    def state(self, path: str) -> FileState | None:
        raise NotImplementedError

    def validate_base_paths(self) -> None:
        raise NotImplementedError

    def ensure_secret_parent(self) -> None:
        raise NotImplementedError

    def create_directory(self, contract: DirectoryContract) -> None:
        raise NotImplementedError

    def create_secret(self, path: str) -> None:
        raise NotImplementedError

    def read_secret(self, path: str) -> bytes:
        raise NotImplementedError


class SystemBackend(HostBackend):
    def assert_privileged(self) -> None:
        if os.geteuid() != 0:
            raise SetupError("must run as root")

    @staticmethod
    def _run(command: Sequence[str]) -> None:
        try:
            subprocess.run(
                command,
                check=True,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.PIPE,
                text=True,
            )
        except (OSError, subprocess.CalledProcessError) as error:
            # Never propagate command output: NSS tools can include local data.
            raise SetupError(f"host identity command failed: {command[0]}") from error

    def group_by_name(self, name: str) -> GroupRecord | None:
        try:
            record = grp.getgrnam(name)
        except KeyError:
            return None
        return GroupRecord(record.gr_name, record.gr_gid)

    def group_by_gid(self, gid: int) -> GroupRecord | None:
        try:
            record = grp.getgrgid(gid)
        except KeyError:
            return None
        return GroupRecord(record.gr_name, record.gr_gid)

    def create_group(self, contract: GroupContract) -> None:
        self._run(
            ("groupadd", "--system", "--gid", str(contract.gid), contract.name)
        )

    @staticmethod
    def _user_record(record: pwd.struct_passwd) -> UserRecord:
        supplementary = frozenset(os.getgrouplist(record.pw_name, record.pw_gid)) - {
            record.pw_gid
        }
        return UserRecord(
            record.pw_name,
            record.pw_uid,
            record.pw_gid,
            record.pw_dir,
            record.pw_shell,
            supplementary,
        )

    def user_by_name(self, name: str) -> UserRecord | None:
        try:
            return self._user_record(pwd.getpwnam(name))
        except KeyError:
            return None

    def user_by_uid(self, uid: int) -> UserRecord | None:
        try:
            return self._user_record(pwd.getpwuid(uid))
        except KeyError:
            return None

    def create_user(self, contract: UserContract) -> None:
        shell = next(
            (candidate for candidate in NONLOGIN_SHELLS if pathlib.Path(candidate).exists()),
            None,
        )
        if shell is None:
            raise SetupError("no supported non-login shell exists")
        group_names = [
            self.group_by_gid(gid).name  # type: ignore[union-attr]
            for gid in sorted(contract.supplementary_gids)
        ]
        command = [
            "useradd",
            "--system",
            "--uid",
            str(contract.uid),
            "--gid",
            str(contract.primary_gid),
            "--no-create-home",
            "--home-dir",
            "/nonexistent",
            "--shell",
            shell,
        ]
        if group_names:
            command.extend(("--groups", ",".join(group_names)))
        command.append(contract.name)
        self._run(command)

    def add_supplementary_groups(self, user: str, gids: frozenset[int]) -> None:
        names = [self.group_by_gid(gid).name for gid in sorted(gids)]  # type: ignore[union-attr]
        self._run(("usermod", "--append", "--groups", ",".join(names), user))

    @staticmethod
    def _safe_run_path(path: str) -> bool:
        candidate = pathlib.PurePosixPath(path)
        if not candidate.is_absolute() or str(candidate) != path or ".." in candidate.parts:
            return False
        if not (path == "/run" or path.startswith("/run/")):
            return False
        current = pathlib.Path("/")
        for component in candidate.parts[1:]:
            current /= component
            try:
                if current.is_symlink():
                    return False
            except OSError:
                return False
            if not current.exists():
                break
        return True

    @staticmethod
    def _filesystem(path: str) -> str:
        target = pathlib.Path(path)
        while not target.exists():
            if target == target.parent:
                raise SetupError("authority path has no existing ancestor")
            target = target.parent
        try:
            result = subprocess.run(
                ("findmnt", "--noheadings", "--output", "FSTYPE", "--target", str(target)),
                check=True,
                capture_output=True,
                text=True,
            )
        except (OSError, subprocess.CalledProcessError) as error:
            raise SetupError("cannot determine authority filesystem") from error
        values = result.stdout.split()
        if len(values) != 1:
            raise SetupError("ambiguous authority filesystem")
        return values[0]

    def state(self, path: str) -> FileState | None:
        if not self._safe_run_path(path):
            return FileState("unsafe", 0, 0, 0, 0, 0, 0, 0, "unknown", False)
        try:
            info = os.lstat(path)
        except FileNotFoundError:
            return None
        if stat.S_ISDIR(info.st_mode):
            kind = "directory"
        elif stat.S_ISREG(info.st_mode):
            kind = "regular"
        elif stat.S_ISLNK(info.st_mode):
            kind = "symlink"
        else:
            kind = "other"
        return FileState(
            kind,
            info.st_uid,
            info.st_gid,
            stat.S_IMODE(info.st_mode),
            info.st_nlink,
            info.st_size,
            info.st_dev,
            info.st_ino,
            self._filesystem(path),
        )

    def validate_base_paths(self) -> None:
        for path in ("/run", "/run/secrets"):
            current = self.state(path)
            if current is None and path == "/run/secrets":
                continue
            if (
                current is None
                or not current.safe_path
                or current.kind != "directory"
                or current.filesystem != "tmpfs"
                or current.uid != 0
                or current.gid != 0
                or current.mode & 0o022
            ):
                raise SetupError(f"{path} is not a protected root-owned tmpfs directory")

    def ensure_secret_parent(self) -> None:
        path = "/run/secrets"
        existing = self.state(path)
        if existing is None:
            os.mkdir(path, 0o700)
            os.chown(path, 0, 0, follow_symlinks=False)
            os.chmod(path, 0o700, follow_symlinks=False)
            existing = self.state(path)
        if (
            existing is None
            or not existing.safe_path
            or existing.kind != "directory"
            or existing.filesystem != "tmpfs"
            or existing.uid != 0
            or existing.gid != 0
            or existing.mode & 0o022
        ):
            raise SetupError("/run/secrets is not a protected root-owned tmpfs directory")

    def create_directory(self, contract: DirectoryContract) -> None:
        try:
            os.mkdir(contract.path, 0o700)
            info = os.lstat(contract.path)
            os.chown(contract.path, contract.uid, contract.gid, follow_symlinks=False)
            os.chmod(contract.path, contract.mode, follow_symlinks=False)
        except OSError as error:
            raise SetupError(f"could not create authority directory {contract.path}") from error
        final = os.lstat(contract.path)
        if (final.st_dev, final.st_ino) != (info.st_dev, info.st_ino):
            raise SetupError(f"authority directory changed during creation: {contract.path}")

    def create_secret(self, path: str) -> None:
        flags = os.O_CREAT | os.O_EXCL | os.O_WRONLY
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        descriptor = -1
        created_inode: tuple[int, int] | None = None
        try:
            descriptor = os.open(path, flags, 0o400)
            info = os.fstat(descriptor)
            created_inode = (info.st_dev, info.st_ino)
            secret = os.urandom(32)
            view = memoryview(secret)
            while view:
                written = os.write(descriptor, view)
                if written <= 0:
                    raise SetupError("secret write made no progress")
                view = view[written:]
            os.fchown(descriptor, 0, 12000)
            os.fchmod(descriptor, 0o440)
            os.fsync(descriptor)
        except (OSError, SetupError) as error:
            if descriptor >= 0:
                os.close(descriptor)
                descriptor = -1
            if created_inode is not None:
                try:
                    current = os.lstat(path)
                    if (current.st_dev, current.st_ino) == created_inode:
                        os.unlink(path)
                except FileNotFoundError:
                    pass
            if isinstance(error, SetupError):
                raise
            raise SetupError(f"could not create secret {path}") from error
        finally:
            if descriptor >= 0:
                os.close(descriptor)
        parent = os.open(str(pathlib.Path(path).parent), os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(parent)
        finally:
            os.close(parent)

    def read_secret(self, path: str) -> bytes:
        flags = os.O_RDONLY
        if hasattr(os, "O_NOFOLLOW"):
            flags |= os.O_NOFOLLOW
        try:
            descriptor = os.open(path, flags)
            try:
                info = os.fstat(descriptor)
                if (
                    not stat.S_ISREG(info.st_mode)
                    or (
                        info.st_uid,
                        info.st_gid,
                        stat.S_IMODE(info.st_mode),
                        info.st_nlink,
                        info.st_size,
                    )
                    != (0, 12000, 0o440, 1, 32)
                ):
                    raise SetupError("secret changed during validation")
                return os.read(descriptor, 33)
            finally:
                os.close(descriptor)
        except OSError as error:
            raise SetupError("secret could not be read safely") from error


def _validate_group(backend: HostBackend, contract: GroupContract, *, required: bool) -> None:
    named = backend.group_by_name(contract.name)
    numbered = backend.group_by_gid(contract.gid)
    if named is None and numbered is None:
        if required:
            raise SetupError(f"required group is absent: {contract.name}")
        return
    if named != GroupRecord(contract.name, contract.gid) or numbered != named:
        raise SetupError(f"group name/GID collision: {contract.name}/{contract.gid}")


def _validate_user(backend: HostBackend, contract: UserContract, *, required: bool) -> None:
    named = backend.user_by_name(contract.name)
    numbered = backend.user_by_uid(contract.uid)
    if named is None and numbered is None:
        if required:
            raise SetupError(f"required user is absent: {contract.name}")
        return
    if named is None or numbered is None or named != numbered:
        raise SetupError(f"user name/UID collision: {contract.name}/{contract.uid}")
    if (
        named.primary_gid != contract.primary_gid
        or named.supplementary_gids != contract.supplementary_gids
        or named.home != "/nonexistent"
        or named.shell not in NONLOGIN_SHELLS
    ):
        raise SetupError(f"existing service identity is unsafe: {contract.name}")


def _validate_directory(
    backend: HostBackend, contract: DirectoryContract, *, required: bool
) -> None:
    current = backend.state(contract.path)
    if current is None:
        if required:
            raise SetupError(f"required authority directory is absent: {contract.path}")
        return
    if (
        not current.safe_path
        or current.kind != "directory"
        or current.filesystem != "tmpfs"
        or (current.uid, current.gid, current.mode)
        != (contract.uid, contract.gid, contract.mode)
    ):
        raise SetupError(f"existing authority directory is unsafe: {contract.path}")


def _validate_secret(backend: HostBackend, path: str, *, required: bool) -> None:
    current = backend.state(path)
    if current is None:
        if required:
            raise SetupError(f"required secret is absent: {path}")
        return
    if (
        not current.safe_path
        or current.kind != "regular"
        or current.filesystem != "tmpfs"
        or (current.uid, current.gid, current.mode, current.nlink, current.size)
        != (0, 12000, 0o440, 1, 32)
    ):
        raise SetupError(f"existing secret is unsafe: {path}")


def _validate_optional_output(backend: HostBackend, path: str, kind: str) -> None:
    current = backend.state(path)
    if current is None:
        return
    expected_gid = 0 if kind == "metadata" else 12000
    expected_mode = 0o600 if kind == "metadata" else 0o440
    if (
        not current.safe_path
        or current.kind != "regular"
        or current.filesystem != "tmpfs"
        or (current.uid, current.gid, current.mode, current.nlink)
        != (0, expected_gid, expected_mode, 1)
        or current.size <= 0
        or (kind == "metadata" and current.size > 65536)
    ):
        raise SetupError(f"existing {kind} target is unsafe: {path}")


def _validate_unique_authority(backend: HostBackend) -> None:
    paths = (
        [contract.path for contract in DIRECTORIES]
        + list(SECRETS)
        + list(METADATA_TARGETS)
        + list(ATTESTATION_TARGETS)
    )
    states = [(path, backend.state(path)) for path in paths]
    seen: dict[tuple[int, int], str] = {}
    for path, current in states:
        if current is None:
            continue
        identity = (current.device, current.inode)
        if identity in seen:
            raise SetupError(f"authority paths share an inode: {seen[identity]} and {path}")
        seen[identity] = path
    existing_secrets = [path for path in SECRETS if backend.state(path) is not None]
    contents = [backend.read_secret(path) for path in existing_secrets]
    if len(contents) != len(set(contents)):
        raise SetupError("two authority secret files contain the same key")


def _validate_caddy(backend: HostBackend, caddy_user: str) -> UserRecord:
    caddy = backend.user_by_name(caddy_user)
    if caddy is None:
        raise SetupError("requested Caddy identity does not exist")
    if backend.user_by_uid(caddy.uid) != caddy:
        raise SetupError("requested Caddy identity has an ambiguous UID")
    if caddy.primary_gid == 12000 or 12000 in caddy.supplementary_gids:
        raise SetupError("Caddy must never belong to the snapshot session group")
    return caddy


def reconcile(
    backend: HostBackend,
    *,
    apply: bool,
    configure_caddy: bool = False,
    caddy_user: str = "caddy",
) -> None:
    """Preflight, apply missing objects, then validate the complete contract."""

    backend.assert_privileged()

    # Complete the read-only collision/unsafe-material pass before any mutation.
    backend.validate_base_paths()
    for contract in GROUPS:
        _validate_group(backend, contract, required=not apply)
    for contract in USERS:
        _validate_user(backend, contract, required=not apply)
    for contract in DIRECTORIES:
        _validate_directory(backend, contract, required=not apply)
    for path in SECRETS:
        _validate_secret(backend, path, required=not apply)
    for path in METADATA_TARGETS:
        _validate_optional_output(backend, path, "metadata")
    for path in ATTESTATION_TARGETS:
        _validate_optional_output(backend, path, "attestation")
    _validate_unique_authority(backend)
    if configure_caddy:
        caddy = _validate_caddy(backend, caddy_user)
        if not apply and not frozenset({12004, 12005}).issubset(
            caddy.supplementary_gids
        ):
            raise SetupError("Caddy lacks one or more metrics-only groups")

    if apply:
        for contract in GROUPS:
            if backend.group_by_name(contract.name) is None:
                backend.create_group(contract)
            _validate_group(backend, contract, required=True)
        for contract in USERS:
            if backend.user_by_name(contract.name) is None:
                backend.create_user(contract)
            _validate_user(backend, contract, required=True)

        backend.ensure_secret_parent()
        for contract in DIRECTORIES:
            if backend.state(contract.path) is None:
                backend.create_directory(contract)
            _validate_directory(backend, contract, required=True)
        for path in SECRETS:
            if backend.state(path) is None:
                backend.create_secret(path)
            _validate_secret(backend, path, required=True)
        _validate_unique_authority(backend)

        if configure_caddy:
            caddy = _validate_caddy(backend, caddy_user)
            wanted = frozenset({12004, 12005})
            missing = wanted - caddy.supplementary_gids
            if missing:
                backend.add_supplementary_groups(caddy_user, missing)
            caddy = _validate_caddy(backend, caddy_user)
            if not wanted.issubset(caddy.supplementary_gids):
                raise SetupError("Caddy metrics group update did not take effect")


def parse_args(argv: Sequence[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="validate only; require every identity, directory, and secret",
    )
    parser.add_argument(
        "--configure-caddy",
        action="store_true",
        help="explicitly add Caddy to both metrics-only groups",
    )
    parser.add_argument(
        "--caddy-user",
        default="caddy",
        help="existing Caddy service user (only with --configure-caddy)",
    )
    args = parser.parse_args(argv)
    if args.caddy_user != "caddy" and not args.configure_caddy:
        parser.error("--caddy-user requires --configure-caddy")
    return args


def main(argv: Sequence[str] | None = None) -> int:
    args = parse_args(sys.argv[1:] if argv is None else argv)
    try:
        reconcile(
            SystemBackend(),
            apply=not args.check,
            configure_caddy=args.configure_caddy,
            caddy_user=args.caddy_user,
        )
    except SetupError as error:
        print(f"snapshot host authority setup failed: {error}", file=sys.stderr)
        return 1
    action = "validated" if args.check else "prepared"
    caddy = "; Caddy metrics membership configured" if args.configure_caddy else ""
    print(f"snapshot host authority {action}: two isolated tmpfs domains{caddy}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
