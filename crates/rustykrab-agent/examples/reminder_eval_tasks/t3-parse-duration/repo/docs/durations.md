# parse_duration

`parse_duration(text)` returns the total number of seconds, as an `int`, in
a duration written as one or more `<integer><unit>` parts. The units are
`h` (hours), `m` (minutes) and `s` (seconds).

- Parts appear in the order h, m, s, and each unit at most once:
  `"1h30m"` is 5400, `"45m"` is 2700, `"2h5s"` is 7205, `"90s"` is 90.
- Leading and trailing whitespace is ignored. There is no whitespace inside
  a duration.
- Units may be upper case: `"1H"` is 3600, `"1H30M"` is 5400.
- `"0s"` is 0.

Anything else raises `ValueError`:

- an empty or all-whitespace string;
- a unit with no number (`"h"`, `"1hm"`);
- an unknown unit (`"5x"`, `"1d"`);
- a repeated or out-of-order unit (`"1h1h"`, `"1m1h"`);
- a number with no unit (`"15"`, `"1h30"`);
- a sign or a decimal point (`"-5m"`, `"1.5h"`).
