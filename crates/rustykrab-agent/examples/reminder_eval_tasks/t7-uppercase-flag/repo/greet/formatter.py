"""Render a greeting."""


def format_greeting(name, cfg):
    return "%s, %s%s" % (cfg["greeting"], name, cfg["punctuation"])
