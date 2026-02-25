#!/usr/bin/env python3
import importlib.util
import os
import unittest
from pathlib import Path
from unittest import mock

MODULE_PATH = Path(__file__).with_name("integration_test.py")
SPEC = importlib.util.spec_from_file_location("integration_test_module", MODULE_PATH)
integration_test = importlib.util.module_from_spec(SPEC)
assert SPEC is not None and SPEC.loader is not None
SPEC.loader.exec_module(integration_test)


class _StdoutMock:
    def __init__(self, is_tty: bool):
        self._is_tty = is_tty

    def isatty(self) -> bool:
        return self._is_tty


class ColorModeTests(unittest.TestCase):
    def test_color_always_ignores_environment(self):
        with mock.patch.dict(os.environ, {"NO_COLOR": "1", "TERM": "dumb"}, clear=True):
            with mock.patch.object(integration_test.sys, "stdout", _StdoutMock(False)):
                self.assertTrue(integration_test.color_output_enabled("always"))

    def test_color_never_disables_output(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            with mock.patch.object(integration_test.sys, "stdout", _StdoutMock(True)):
                self.assertFalse(integration_test.color_output_enabled("never"))

    def test_color_auto_respects_no_color(self):
        with mock.patch.dict(os.environ, {"NO_COLOR": "1", "TERM": "xterm-256color"}, clear=True):
            with mock.patch.object(integration_test.sys, "stdout", _StdoutMock(True)):
                self.assertFalse(integration_test.color_output_enabled("auto"))

    def test_color_auto_respects_term_dumb(self):
        with mock.patch.dict(os.environ, {"TERM": "dumb"}, clear=True):
            with mock.patch.object(integration_test.sys, "stdout", _StdoutMock(True)):
                self.assertFalse(integration_test.color_output_enabled("auto"))

    def test_color_auto_uses_tty_when_env_allows(self):
        with mock.patch.dict(os.environ, {"TERM": "xterm-256color"}, clear=True):
            with mock.patch.object(integration_test.sys, "stdout", _StdoutMock(True)):
                self.assertTrue(integration_test.color_output_enabled("auto"))

    def test_color_auto_disables_when_not_tty(self):
        with mock.patch.dict(os.environ, {"TERM": "xterm-256color"}, clear=True):
            with mock.patch.object(integration_test.sys, "stdout", _StdoutMock(False)):
                self.assertFalse(integration_test.color_output_enabled("auto"))


if __name__ == "__main__":
    unittest.main()
