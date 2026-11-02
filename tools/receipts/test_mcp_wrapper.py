"""The wrapper's verdicts on receipt streams recorded from real drivers.

Each file in testdata/ is a `mcp_wrapper.py --trace` recording from
`just acceptance wrapper-t6a`: the focus and window list when the wrapper
started, then every signal in the order it arrived, with the start and end of
each tool call. The tests replay a recording through the wrapper's Checker
and compare its verdicts with what the receipts show happened.

- alert.jsonl: cua-driver 0.38.0 types T6a's name and email, then the phone
  number after a page alert() opened over the form.
- popup.jsonl: computer-use-linux 292376c, the same with a window.open popup.
- click.jsonl: cua-driver clicks the session's terminal and types into it,
  clicks settle.html, which repaints for 1.2 s, and clicks it again, which
  starts an endless animation, with a 1500 ms settle timeout.

    python3 -m unittest discover -s tools/receipts
"""

import json
import unittest
from pathlib import Path

from mcp_wrapper import FAILED, Checker, annotate

DATA = Path(__file__).resolve().parent / "testdata"


def replay(name):
    """[(tool, report)] for each checked call in the recording."""
    checker, verdicts, tool = None, [], None
    for line in (DATA / name).read_text().splitlines():
        event = json.loads(line)
        kind = event["event"]
        if kind == "start":
            checker = Checker(event["focus"], event["windows"])
        elif kind == "signal":
            checker.observe(event["record"])
        elif kind == "call":
            tool = event["tool"]
            checker.begin(tool, event["arguments"], event["typing"])
        elif kind == "finish":
            verdicts.append((tool, checker.finish(event["focus"], event["settle"])))
    return verdicts


def failed(verdicts):
    return [(tool, report.failures) for tool, report in verdicts if report.failures]


class AlertTest(unittest.TestCase):
    """A page alert() opens 300 ms after Tab focuses Phone. It moves no
    surface focus; the form's text field turns `none` with no input to explain
    it, and the phone number goes to the alert."""

    def test_only_the_phone_number_fails(self):
        verdicts = replay("alert.jsonl")
        self.assertEqual(
            [tool for tool, _ in verdicts],
            ["type_text", "press_key", "type_text", "press_key", "type_text"],
        )
        self.assertEqual(failed(verdicts[:4]), [])
        self.assertEqual(len(verdicts[4][1].failures), 1)
        self.assertRegex(
            verdicts[4][1].failures[0],
            r'^9 of 9 keys went to "Registration form - Chromium" .* after its text field'
            r" lost focus without input from you: .*alert\(\)",
        )

    def test_the_failure_is_loud(self):
        _, report = replay("alert.jsonl")[4]
        result = annotate({"content": [{"type": "text", "text": "typed"}]}, report, 1)
        self.assertTrue(result["isError"])
        self.assertTrue(result["content"][0]["text"].startswith(FAILED))
        self.assertEqual(result["content"][1]["text"], "typed")


class PopupTest(unittest.TestCase):
    """A window.open popup opens 300 ms after Tab focuses Phone and takes
    keyboard focus by activation; the phone number goes to it."""

    def test_only_the_phone_number_fails_and_names_the_popup(self):
        verdicts = replay("popup.jsonl")
        self.assertEqual(failed(verdicts[:4]), [])
        self.assertEqual(len(verdicts[4][1].failures), 1)
        self.assertRegex(
            verdicts[4][1].failures[0],
            r'^8 of 8 keys went to "Session expiring - Chromium" .*, not to'
            r' "Registration form - Chromium" .* It took focus by itself \(activation\)',
        )


class OwnClickTest(unittest.TestCase):
    """The driver clicks the terminal, behind the page that has focus, and
    types into it. Its own click moved focus, so nothing is misdirected."""

    def test_no_false_alarm(self):
        verdicts = replay("click.jsonl")
        self.assertEqual([tool for tool, _ in verdicts], ["click", "type_text", "click", "click"])
        self.assertEqual(failed(verdicts), [])
        self.assertRegex(verdicts[0][1].summary["focus_changes"][0], r'^"acceptance" .*\(click\)$')
        self.assertEqual(list(verdicts[1][1].summary["delivered_to"]), [verdicts[1][1].summary["focus"]])
        self.assertTrue(verdicts[1][1].summary["focus"].startswith('"acceptance" (foot'))


class SettleTimeoutTest(unittest.TestCase):
    """The page settles after its 1.2 s of repaints; after the second click
    it animates forever, so the wait times out and the result says so."""

    def test_the_timeout_is_reported(self):
        verdicts = replay("click.jsonl")
        _, repaint = verdicts[2]
        _, endless = verdicts[3]
        self.assertEqual(repaint.summary["settle"]["result"], "settled")
        self.assertEqual(repaint.notes, [])
        self.assertEqual(endless.failures, [])
        self.assertEqual(endless.summary["settle"]["result"], "timeout")
        self.assertEqual(len(endless.notes), 1)
        self.assertRegex(endless.notes[0], r"^The screen was still changing \d+ ms after the input")


if __name__ == "__main__":
    unittest.main()
