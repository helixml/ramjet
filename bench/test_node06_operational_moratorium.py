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
        # caller, not just the run it was lifted for. The supervised window of
        # 2026-08-14 is closed, so the armed state is the committed default and
        # any future run needs its own reviewed lift.
        self.assertTrue(moratorium.MORATORIUM_ACTIVE)
        with self.assertRaises(moratorium.MoratoriumError):
            moratorium.require_active_work_permitted("gpu-workload.ramped-load")

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
