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

    def test_current_state_is_the_reviewed_supervised_lift(self):
        # Guards the operational fact that the flag is global while lifted, so
        # a stale False is standing permission for every caller. If this fails,
        # the supervised window is over and the moratorium should be re-armed.
        self.assertFalse(moratorium.MORATORIUM_ACTIVE)
        self.assertIsNone(
            moratorium.require_active_work_permitted("gpu-workload.ramped-load")
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
