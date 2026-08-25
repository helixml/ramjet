import unittest
from unittest import mock

import node06_operational_moratorium as moratorium


class Node06OperationalMoratoriumTests(unittest.TestCase):
    def test_reactivated_moratorium_fails_closed_with_bounded_reason(self):
        # The lift is a state, not a removal: re-arming must restore the
        # fail-closed behaviour exactly, so this asserts the mechanism rather
        # than the flag's current value.
        with mock.patch.object(moratorium, "MORATORIUM_ACTIVE", True):
            with self.assertRaisesRegex(moratorium.MoratoriumError, "moratorium"):
                moratorium.require_active_work_permitted("gpu-workload.focused-smoke")
        self.assertEqual(
            moratorium.MORATORIUM_REASON,
            "cooling_ac_failure_2026_08_14",
        )

    def test_default_state_is_retired(self):
        self.assertFalse(moratorium.MORATORIUM_ACTIVE)
        self.assertIsNone(
            moratorium.require_active_work_permitted("gpu-workload.rc6-gate")
        )

    def test_only_reviewed_inactive_state_permits_bounded_operation(self):
        with mock.patch.object(moratorium, "MORATORIUM_ACTIVE", False):
            self.assertIsNone(
                moratorium.require_active_work_permitted(
                    "p2p-full-prerequisite"
                )
            )

    def test_operation_name_is_bounded_even_after_lift(self):
        with mock.patch.object(moratorium, "MORATORIUM_ACTIVE", False):
            for operation in ("", "UPPER", "x" * 97, "gpu workload"):
                with self.subTest(operation=operation), self.assertRaises(
                    moratorium.MoratoriumError
                ):
                    moratorium.require_active_work_permitted(operation)


class AuthorizedWindowTests(unittest.TestCase):
    def test_an_unnamed_environment_is_irrelevant_after_retirement(self):
        with mock.patch.dict("os.environ", {}, clear=True):
            self.assertIsNone(
                moratorium.require_active_work_permitted("gpu-workload.smoke")
            )

    def test_an_unknown_window_name_is_irrelevant_after_retirement(self):
        with mock.patch.dict(
            "os.environ", {moratorium.ENV_AUTHORIZATION: "made-up"}, clear=True
        ):
            self.assertIsNone(moratorium.active_authorization())
            self.assertIsNone(
                moratorium.require_active_work_permitted("gpu-workload.smoke")
            )

    def test_a_reviewed_window_permits_the_run_and_carries_its_bounds(self):
        with mock.patch.dict(
            "os.environ",
            {moratorium.ENV_AUTHORIZATION: "supervised-2026-08-14"},
            clear=True,
        ):
            self.assertIsNone(
                moratorium.require_active_work_permitted("gpu-workload.smoke")
            )
            window = moratorium.active_authorization()
        self.assertEqual(window.max_abort_c, 55)
        self.assertEqual(window.max_runtime_seconds, 1500)

    def test_every_window_bounds_intake_air_not_silicon(self):
        # The gate is chassis intake air, so the ceiling lives on a room-scale
        # temperature. A window carrying a GPU-scale number would silently
        # never fire: intake air does not reach 78C before the room is lost.
        for window in moratorium.AUTHORIZED_WINDOWS.values():
            self.assertLessEqual(window.max_abort_c, 55, window.identifier)
            self.assertLessEqual(window.max_runtime_seconds, 1500, window.identifier)
