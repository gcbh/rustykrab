# greet

    python3 -m greet.cli Ada                 # Hello, Ada!
    python3 -m greet.cli --greeting Hi Ada   # Hi, Ada!

Defaults live in `greet/config.py`; `greet/formatter.py` renders the
greeting; `greet/cli.py` is the command line. Run the tests with
`python3 -m unittest discover`.
