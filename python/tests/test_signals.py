"""The helper's signal handler: one Shutdown per shutdown (WP3 bench finding)."""
import signal
import types
import unittest

import _path  # noqa: F401  (puts python/ on sys.path)
from obd.main import make_signal_handler
from obd.session import Shutdown


class SignalHandlerTests(unittest.TestCase):
    def test_first_signal_raises_shutdown_with_its_number(self):
        handler = make_signal_handler(types.SimpleNamespace(closed=False))
        with self.assertRaises(Shutdown) as cm:
            handler(signal.SIGINT, None)
        self.assertEqual(cm.exception.args[0], signal.SIGINT)

    def test_signal_after_the_shutdown_close_is_ignored(self):
        # The agent sends SIGINT; systemd SIGTERMs the cgroup while the final
        # "stopped" status / audit lines are being flushed. No second Shutdown.
        session = types.SimpleNamespace(closed=True)
        handler = make_signal_handler(session)
        handler(signal.SIGTERM, None)   # must not raise
        handler(signal.SIGINT, None)


if __name__ == "__main__":
    unittest.main()
