"""Unit tests for the AIDL catalog builder. Run: python3 -m unittest -v (from catalog/)."""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

import bindfetto_catalog as bc  # noqa: E402

FIXTURES = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures")


class BuilderTest(unittest.TestCase):
    def test_sequential_codes_and_multiline(self):
        cat = bc.build_catalog([os.path.join(FIXTURES, "IActivityManager.aidl")])
        self.assertEqual(
            cat["android.app.IActivityManager"],
            {1: "getTasks", 2: "startActivity", 3: "noteWakeupAlarm"},
        )

    def test_explicit_codes(self):
        cat = bc.build_catalog([os.path.join(FIXTURES, "ITricky.aidl")])
        self.assertEqual(cat["com.example.IExplicit"], {5: "alpha", 10: "beta"})

    def test_skips_consts_and_nested_types(self):
        cat = bc.build_catalog([os.path.join(FIXTURES, "ITricky.aidl")])
        # getName=1, setValues=2, echo=3, ping=4 — VERSION/NAME consts and the nested
        # parcelable must not consume transaction codes.
        self.assertEqual(
            cat["com.example.ITricky"],
            {1: "getName", 2: "setValues", 3: "echo", 4: "ping"},
        )

    def test_directory_recursion_merges_all(self):
        cat = bc.build_catalog([FIXTURES])
        self.assertEqual(
            set(cat),
            {
                "android.app.IActivityManager",
                "com.example.ITricky",
                "com.example.IExplicit",
            },
        )

    def test_with_args_emits_v2_entries(self):
        cat = bc.build_catalog(
            [os.path.join(FIXTURES, "ITricky.aidl")], with_args=True
        )
        # echo(@nullable String s) — annotation and direction stripped, type kept.
        self.assertEqual(
            cat["com.example.ITricky"][3],
            {"name": "echo", "args": [{"name": "s", "type": "String"}]},
        )
        # setValues(in int[] vals, inout Map<String,String> m) — commas inside the
        # generic must not split the parameter list; array/generic types kept verbatim.
        self.assertEqual(
            cat["com.example.ITricky"][2],
            {
                "name": "setValues",
                "args": [
                    {"name": "vals", "type": "int[]"},
                    {"name": "m", "type": "Map<String, String>"},
                ],
            },
        )
        # A no-arg method still gets an (empty) args list in v2.
        self.assertEqual(cat["com.example.ITricky"][4], {"name": "ping", "args": []})

    def test_json_is_sorted_and_string_keyed(self):
        cat = bc.build_catalog([os.path.join(FIXTURES, "IActivityManager.aidl")])
        text = bc.to_json(cat)
        self.assertIn('"1": "getTasks"', text)
        self.assertIn('"7"', bc.to_json({"X": {7: "a", 1: "b"}}))
        # numeric-sorted keys within an interface
        self.assertLess(text.index('"1"'), text.index('"3"'))


if __name__ == "__main__":
    unittest.main()
