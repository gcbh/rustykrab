# Changelog

All notable changes to tinyqueue are recorded here.

## Unreleased

- Add `BoundedQueue.drain()`, which removes and returns every item.
- `push()` returns `False` instead of raising when the queue is full.

## 1.3.2 - 2026-08-30

- Fix `peek()` on an empty queue raising `IndexError` instead of returning
  `None`.

## 1.3.1 - 2026-07-12

- Reject a capacity below 1 with `ValueError`.
