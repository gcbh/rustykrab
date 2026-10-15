"""BoundedQueue: a FIFO that refuses items past its capacity."""

from collections import deque


class BoundedQueue:
    def __init__(self, capacity):
        if capacity < 1:
            raise ValueError("capacity must be at least 1")
        self.capacity = capacity
        self._items = deque()

    def __len__(self):
        return len(self._items)

    def push(self, item):
        """Append item; return False (and drop it) when the queue is full."""
        if len(self._items) >= self.capacity:
            return False
        self._items.append(item)
        return True

    def pop(self):
        return self._items.popleft()

    def peek(self):
        """The oldest item, or None when the queue is empty."""
        return self._items[0] if self._items else None

    def drain(self):
        """Remove and return every item, oldest first."""
        items = list(self._items)
        self._items.clear()
        return items
