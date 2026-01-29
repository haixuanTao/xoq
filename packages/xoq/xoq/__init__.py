"""XOQ - Remote peripherals over P2P."""

# Re-export xoq-can as xoq.can
try:
    import can
    can = can
except ImportError:
    can = None

# Re-export xoq-opencv as xoq.cv2
try:
    import cv2
    cv2 = cv2
except ImportError:
    cv2 = None

# Re-export xoq-serial as xoq.serial
try:
    import serial
    serial = serial
except ImportError:
    serial = None

__version__ = "0.3.4"
__all__ = ["can", "cv2", "serial"]
