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

    def test_default_state_is_armed(self):
        # The flag is global: a stale False is standing permission for every
        # caller, not just the run it was lifted for. Both 2026-08-14 windows
        # are closed, so armed is the committed default and any future run
        # needs its own reviewed lift.
        self.assertTrue(moratorium.MORATORIUM_ACTIVE)
        with self.assertRaises(moratorium.MoratoriumError):
            moratorium.require_active_work_permitted("gpu-workload.rc6-gate")

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
    def test_an_unnamed_environment_still_fails_closed(self):
        with mock.patch.dict("os.environ", {}, clear=True):
            with self.assertRaises(moratorium.MoratoriumError):
                moratorium.require_active_work_permitted("gpu-workload.smoke")

    def test_an_unknown_window_name_fails_closed(self):
        with mock.patch.dict(
            "os.environ", {moratorium.ENV_AUTHORIZATION: "made-up"}, clear=True
        ):
            self.assertIsNone(moratorium.active_authorization())
            with self.assertRaises(moratorium.MoratoriumError):
                moratorium.require_active_work_permitted("gpu-workload.smoke")

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
        self.assertEqual(window.max_abort_c, 84)
        self.assertEqual(window.max_runtime_seconds, 1500)

    def test_every_window_stays_below_hardware_throttle_onset(self):
        # 85C is throttle onset and 90C is shutdown on node06's devices. No
        # reviewed window may authorize a ceiling that measures throttled
        # hardware or that the driver would preempt.
        for window in moratorium.AUTHORIZED_WINDOWS.values():
            self.assertLess(window.max_abort_c, 85, window.identifier)
            self.assertLessEqual(window.max_runtime_seconds, 1500, window.identifier)
