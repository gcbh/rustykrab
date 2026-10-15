"""Greeting defaults, overridable per call."""

DEFAULTS = {
    "greeting": "Hello",
    "punctuation": "!",
}


def load(overrides=None):
    """DEFAULTS with every override that is not None applied."""
    cfg = dict(DEFAULTS)
    if overrides:
        cfg.update({k: v for k, v in overrides.items() if v is not None})
    return cfg
