"""Clock-time helpers."""


def format_hms(seconds):
    """Format a number of seconds as H:MM:SS."""
    if seconds < 0:
        raise ValueError("negative duration")
    hours, rest = divmod(int(seconds), 3600)
    minutes, secs = divmod(rest, 60)
    return "%d:%02d:%02d" % (hours, minutes, secs)
