import contextlib
import importlib.util
import io
import pathlib
import sys
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
MODULE_PATH = (
    ROOT / "deploy" / "dspark_0731" / "setup_snapshot_production_host.py"
)
SPEC = importlib.util.spec_from_file_location("snapshot_host_authority", MODULE_PATH)
assert SPEC is not None and SPEC.loader is not None
authority = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = authority
SPEC.loader.exec_module(authority)


class FakeBackend(authority.HostBackend):
    def __init__(self):
        self.groups_by_name = {}
        self.groups_by_gid = {}
        self.users_by_name = {}
        self.users_by_uid = {}
        self.files = {}
        self.secret_contents = {}
        self.operations = []
        self.next_inode = 100

    def assert_privileged(self):
        return None

    def group_by_name(self, name):
        return self.groups_by_name.get(name)

    def group_by_gid(self, gid):
        return self.groups_by_gid.get(gid)

    def put_group(self, name, gid):
        record = authority.GroupRecord(name, gid)
        self.groups_by_name[name] = record
        self.groups_by_gid[gid] = record

    def create_group(self, contract):
        self.operations.append(("create_group", contract.name))
        self.put_group(contract.name, contract.gid)

    def user_by_name(self, name):
        return self.users_by_name.get(name)

    def user_by_uid(self, uid):
        return self.users_by_uid.get(uid)

    def put_user(
        self,
        name,
        uid,
        primary_gid,
        supplementary=frozenset(),
        home="/nonexistent",
        shell="/usr/sbin/nologin",
    ):
        record = authority.UserRecord(
            name, uid, primary_gid, home, shell, frozenset(supplementary)
        )
        self.users_by_name[name] = record
        self.users_by_uid[uid] = record

    def create_user(self, contract):
        self.operations.append(("create_user", contract.name))
        self.put_user(
            contract.name,
            contract.uid,
            contract.primary_gid,
            contract.supplementary_gids,
        )

    def add_supplementary_groups(self, user, gids):
        self.operations.append(("add_groups", user, tuple(sorted(gids))))
        current = self.users_by_name[user]
        self.put_user(
            current.name,
            current.uid,
            current.primary_gid,
            current.supplementary_gids | gids,
            current.home,
            current.shell,
        )

    def state(self, path):
        return self.files.get(path)

    def validate_base_paths(self):
        return None

    def _put_file(
        self,
        path,
        kind,
        uid,
        gid,
        mode,
        *,
        size=0,
        nlink=1,
        filesystem="tmpfs",
        safe_path=True,
        inode=None,
    ):
        if inode is None:
            inode = self.next_inode
            self.next_inode += 1
        self.files[path] = authority.FileState(
            kind,
            uid,
            gid,
            mode,
            nlink,
            size,
            7,
            inode,
            filesystem,
            safe_path,
        )

    def ensure_secret_parent(self):
        self.operations.append(("ensure_secret_parent",))

    def create_directory(self, contract):
        self.operations.append(("create_directory", contract.path))
        self._put_file(
            contract.path,
            "directory",
            contract.uid,
            contract.gid,
            contract.mode,
        )

    def create_secret(self, path):
        self.operations.append(("create_secret", path))
        content = bytes([len(self.secret_contents) + 1]) * 32
        self.secret_contents[path] = content
        self._put_file(path, "regular", 0, 12000, 0o440, size=32)

    def read_secret(self, path):
        return self.secret_contents[path]


class SnapshotHostAuthorityTests(unittest.TestCase):
    def test_system_backend_requests_explicit_system_accounts(self):
        backend = authority.SystemBackend()
        commands = []
        backend._run = commands.append
        groups = {
            group.gid: authority.GroupRecord(group.name, group.gid)
            for group in authority.GROUPS
        }
        backend.group_by_gid = groups.get

        backend.create_group(authority.SESSION_GROUP)
        backend.create_user(authority.USERS[0])

        self.assertEqual(
            commands[0],
            (
                "groupadd",
                "--system",
                "--gid",
                "12000",
                "ramjet-snapshot",
            ),
        )
        self.assertEqual(
            commands[1][0:5],
            ["useradd", "--system", "--uid", "12001", "--gid"],
        )
        self.assertIn("--no-create-home", commands[1])
        self.assertIn("--shell", commands[1])
        self.assertIn("--groups", commands[1])

    def test_first_apply_creates_exact_contract_and_is_idempotent(self):
        backend = FakeBackend()
        authority.reconcile(backend, apply=True)

        self.assertEqual(
            {group.gid for group in backend.groups_by_name.values()},
            {12000, 12004, 12005},
        )
        self.assertEqual(
            {user.uid for user in backend.users_by_name.values()},
            {12001, 12002, 12003},
        )
        self.assertEqual(
            backend.user_by_uid(12001).supplementary_gids, frozenset({12004})
        )
        self.assertEqual(
            backend.user_by_uid(12002).supplementary_gids, frozenset()
        )
        self.assertEqual(
            backend.user_by_uid(12003).supplementary_gids, frozenset({12005})
        )
        self.assertEqual(len(backend.secret_contents), 4)
        self.assertEqual(len(set(backend.secret_contents.values())), 4)
        self.assertTrue(
            all(path not in backend.files for path in authority.METADATA_TARGETS)
        )
        self.assertTrue(
            all(path not in backend.files for path in authority.ATTESTATION_TARGETS)
        )

        operations = list(backend.operations)
        contents = dict(backend.secret_contents)
        authority.reconcile(backend, apply=True)
        self.assertEqual(
            backend.operations[len(operations) :], [("ensure_secret_parent",)]
        )
        self.assertEqual(backend.secret_contents, contents)
        authority.reconcile(backend, apply=False)

    def test_group_name_or_gid_collision_fails_before_mutation(self):
        for name, gid in (
            (authority.SESSION_GROUP.name, 22000),
            ("occupied", authority.SESSION_GROUP.gid),
        ):
            with self.subTest(name=name, gid=gid):
                backend = FakeBackend()
                backend.put_group(name, gid)
                with self.assertRaisesRegex(authority.SetupError, "collision"):
                    authority.reconcile(backend, apply=True)
                self.assertEqual(backend.operations, [])

    def test_user_collision_and_unsafe_existing_identity_fail(self):
        cases = (
            ("ramjet", 22002, 12000, frozenset()),
            ("occupied", 12002, 12000, frozenset()),
            ("ramjet", 12002, 12000, frozenset({12004})),
            ("ramjet", 12002, 12004, frozenset()),
        )
        for name, uid, primary, supplementary in cases:
            with self.subTest(name=name, uid=uid, primary=primary):
                backend = FakeBackend()
                backend.put_user(name, uid, primary, supplementary)
                with self.assertRaises(authority.SetupError):
                    authority.reconcile(backend, apply=True)
                self.assertEqual(backend.operations, [])

    def test_existing_unsafe_path_fails_before_identity_creation(self):
        backend = FakeBackend()
        contract = authority.DIRECTORIES[0]
        backend._put_file(
            contract.path,
            "directory",
            contract.uid,
            contract.gid,
            0o2770,
        )
        with self.assertRaisesRegex(authority.SetupError, "directory is unsafe"):
            authority.reconcile(backend, apply=True)
        self.assertEqual(backend.operations, [])

    def test_symlink_and_non_tmpfs_authority_fail_closed(self):
        for safe_path, filesystem in ((False, "tmpfs"), (True, "ext4")):
            with self.subTest(safe_path=safe_path, filesystem=filesystem):
                backend = FakeBackend()
                contract = authority.DIRECTORIES[2]
                backend._put_file(
                    contract.path,
                    "directory",
                    contract.uid,
                    contract.gid,
                    contract.mode,
                    safe_path=safe_path,
                    filesystem=filesystem,
                )
                with self.assertRaises(authority.SetupError):
                    authority.reconcile(backend, apply=True)
                self.assertEqual(backend.operations, [])

    def test_invalid_or_reused_existing_secret_is_never_overwritten(self):
        backend = FakeBackend()
        path = authority.SECRETS[0]
        backend._put_file(path, "regular", 0, 12000, 0o440, size=31)
        backend.secret_contents[path] = b"x" * 31
        with self.assertRaisesRegex(authority.SetupError, "secret is unsafe"):
            authority.reconcile(backend, apply=True)
        self.assertEqual(backend.operations, [])

        backend = FakeBackend()
        for path in authority.SECRETS[:2]:
            backend._put_file(path, "regular", 0, 12000, 0o440, size=32)
            backend.secret_contents[path] = b"same secret".ljust(32, b"x")
        with self.assertRaisesRegex(authority.SetupError, "same key"):
            authority.reconcile(backend, apply=True)
        self.assertEqual(backend.operations, [])

    def test_existing_metadata_must_be_bounded_root_only_tmpfs(self):
        backend = FakeBackend()
        backend._put_file(
            authority.METADATA_TARGETS[0],
            "regular",
            0,
            0,
            0o644,
            size=100,
        )
        with self.assertRaisesRegex(authority.SetupError, "metadata target is unsafe"):
            authority.reconcile(backend, apply=True)
        self.assertEqual(backend.operations, [])

    def test_hard_linked_authority_paths_fail_before_mutation(self):
        backend = FakeBackend()
        first, second = authority.SECRETS[:2]
        for path in (first, second):
            backend._put_file(
                path, "regular", 0, 12000, 0o440, size=32, nlink=1, inode=91
            )
            backend.secret_contents[path] = bytes(path[-1], "ascii") * 32
        with self.assertRaisesRegex(authority.SetupError, "share an inode"):
            authority.reconcile(backend, apply=True)
        self.assertEqual(backend.operations, [])

    def test_check_requires_complete_managed_contract_but_not_metadata_yet(self):
        backend = FakeBackend()
        with self.assertRaisesRegex(authority.SetupError, "required group is absent"):
            authority.reconcile(backend, apply=False)
        authority.reconcile(backend, apply=True)
        authority.reconcile(backend, apply=False)

    def test_caddy_membership_is_opt_in_and_never_grants_session_authority(self):
        backend = FakeBackend()
        backend.put_user("caddy", 998, 998, frozenset({999}))
        authority.reconcile(backend, apply=True)
        self.assertNotIn("add_groups", [operation[0] for operation in backend.operations])

        with self.assertRaisesRegex(authority.SetupError, "lacks"):
            authority.reconcile(
                backend, apply=False, configure_caddy=True, caddy_user="caddy"
            )

        authority.reconcile(backend, apply=True, configure_caddy=True)
        caddy = backend.user_by_name("caddy")
        self.assertEqual(caddy.supplementary_gids, frozenset({999, 12004, 12005}))
        self.assertNotIn(12000, caddy.supplementary_gids)
        authority.reconcile(
            backend, apply=False, configure_caddy=True, caddy_user="caddy"
        )

        poisoned = FakeBackend()
        poisoned.put_user("caddy", 998, 998, frozenset({12000}))
        with self.assertRaisesRegex(authority.SetupError, "must never belong"):
            authority.reconcile(poisoned, apply=True, configure_caddy=True)
        self.assertEqual(poisoned.operations, [])

    def test_custom_caddy_name_requires_explicit_opt_in(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                authority.parse_args(["--caddy-user", "www-data"])


if __name__ == "__main__":
    unittest.main()
