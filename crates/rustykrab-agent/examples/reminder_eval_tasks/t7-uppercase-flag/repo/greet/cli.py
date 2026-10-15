"""Command line: python3 -m greet.cli [--greeting G] [--punctuation P] NAME"""

import argparse
import sys

from greet import config
from greet.formatter import format_greeting


def main(argv=None):
    parser = argparse.ArgumentParser(prog="greet")
    parser.add_argument("name")
    parser.add_argument("--greeting")
    parser.add_argument("--punctuation")
    args = parser.parse_args(argv)
    cfg = config.load({"greeting": args.greeting, "punctuation": args.punctuation})
    print(format_greeting(args.name, cfg))
    return 0


if __name__ == "__main__":
    sys.exit(main())
