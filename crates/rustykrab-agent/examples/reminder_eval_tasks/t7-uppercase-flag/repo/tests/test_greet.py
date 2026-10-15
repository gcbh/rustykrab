import io
import unittest
from contextlib import redirect_stdout

from greet import config
from greet.cli import main
from greet.formatter import format_greeting


class FormatterTest(unittest.TestCase):
    def test_default(self):
        self.assertEqual(format_greeting("Ada", config.load()), "Hello, Ada!")

    def test_override(self):
        cfg = config.load({"greeting": "Hi", "punctuation": None})
        self.assertEqual(format_greeting("Bo", cfg), "Hi, Bo!")


class CliTest(unittest.TestCase):
    def test_cli(self):
        out = io.StringIO()
        with redirect_stdout(out):
            main(["--greeting", "Hey", "Cy"])
        self.assertEqual(out.getvalue(), "Hey, Cy!\n")


if __name__ == "__main__":
    unittest.main()
