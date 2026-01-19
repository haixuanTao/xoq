"""Type stubs for xoq Python bindings."""

from __future__ import annotations

import numpy as np
import numpy.typing as npt

# MoQ classes
class MoqConnection:
    def __init__(
        self,
        path: str | None = None,
        token: str | None = None,
        relay: str | None = None,
    ) -> None: ...
    def create_track(self, name: str) -> MoqTrackWriter: ...
    def subscribe_track(self, track_name: str) -> MoqTrackReader | None: ...

class MoqPublisher:
    def __init__(
        self,
        path: str | None = None,
        token: str | None = None,
        relay: str | None = None,
    ) -> None: ...
    def create_track(self, name: str) -> MoqTrackWriter: ...

class MoqSubscriber:
    def __init__(
        self,
        path: str | None = None,
        token: str | None = None,
        relay: str | None = None,
    ) -> None: ...
    def subscribe_track(self, track_name: str) -> MoqTrackReader | None: ...

class MoqTrackWriter:
    def write(self, data: bytes) -> None: ...
    def write_str(self, data: str) -> None: ...

class MoqTrackReader:
    def read(self) -> bytes | None: ...
    def read_string(self) -> str | None: ...

# Iroh classes
class IrohServer:
    def __init__(
        self,
        identity_path: str | None = None,
        alpn: bytes | None = None,
    ) -> None: ...
    def id(self) -> str: ...
    def accept(self) -> IrohConnection | None: ...

class IrohConnection:
    def __init__(
        self,
        server_id: str,
        alpn: bytes | None = None,
    ) -> None: ...
    def remote_id(self) -> str: ...
    def open_stream(self) -> IrohStream: ...
    def accept_stream(self) -> IrohStream: ...

class IrohStream:
    def write(self, data: bytes) -> None: ...
    def write_str(self, data: str) -> None: ...
    def read(self, size: int = 4096) -> bytes | None: ...
    def read_string(self) -> str | None: ...

# Serial bridge classes
class Server:
    def __init__(
        self,
        port: str,
        baud_rate: int = 115200,
        identity_path: str | None = None,
    ) -> None: ...
    def id(self) -> str: ...
    def run(self) -> None: ...
    def run_once(self) -> None: ...

class Serial:
    """pyserial-compatible interface to a remote serial port."""

    in_waiting: int
    is_open: bool
    timeout: float | None
    port: str
    name: str

    def __init__(self, port: str, timeout: float | None = None) -> None: ...
    def write(self, data: bytes) -> int: ...
    def read(self, size: int = 1) -> bytes: ...
    def readline(self) -> bytes: ...
    def reset_input_buffer(self) -> None: ...
    def read_until(self, terminator: bytes | None = None) -> bytes: ...
    def flush(self) -> None: ...
    def close(self) -> None: ...
    def __enter__(self) -> Serial: ...
    def __exit__(
        self,
        exc_type: type | None,
        exc_val: BaseException | None,
        exc_tb: object | None,
    ) -> bool: ...

# Camera bridge classes
class CameraServer:
    """A server that streams camera frames to remote clients over iroh P2P."""

    def __init__(
        self,
        camera_index: int = 0,
        width: int = 640,
        height: int = 480,
        fps: int = 30,
        identity_path: str | None = None,
    ) -> None: ...
    def id(self) -> str: ...
    def run(self) -> None: ...
    def run_once(self) -> None: ...

# OpenCV-compatible VideoCapture and property constants
CAP_PROP_POS_MSEC: int
CAP_PROP_POS_FRAMES: int
CAP_PROP_FRAME_WIDTH: int
CAP_PROP_FRAME_HEIGHT: int
CAP_PROP_FPS: int
CAP_PROP_FRAME_COUNT: int

class VideoCapture:
    """OpenCV-compatible VideoCapture for remote cameras over iroh P2P.

    Drop-in replacement for cv2.VideoCapture that connects to a remote
    camera server instead of a local device.

    Example:
        # Instead of: cap = cv2.VideoCapture(0)
        cap = xoq.VideoCapture("server-endpoint-id")

        while True:
            ret, frame = cap.read()
            if not ret:
                break
            cv2.imshow('Remote Camera', frame)
            if cv2.waitKey(1) & 0xFF == ord('q'):
                break

        cap.release()
    """

    def __init__(self, source: str) -> None:
        """Open a connection to a remote camera server.

        Args:
            source: The server's endpoint ID (replaces device index or URL in OpenCV)
        """
        ...

    def read(self) -> tuple[bool, npt.NDArray[np.uint8] | None]:
        """Read a frame from the remote camera.

        Returns:
            Tuple of (success, frame) where frame is a numpy array in BGR format
            with shape (height, width, 3), or None if read failed.
        """
        ...

    def grab(self) -> bool:
        """Grab a frame from the remote camera (without decoding).

        Returns:
            True if a frame was grabbed successfully.
        """
        ...

    def retrieve(self) -> tuple[bool, npt.NDArray[np.uint8] | None]:
        """Retrieve the grabbed frame.

        Returns:
            Tuple of (success, frame) where frame is a numpy array in BGR format.
        """
        ...

    def isOpened(self) -> bool:
        """Check if the connection is open."""
        ...

    def release(self) -> None:
        """Release the connection."""
        ...

    def get(self, prop_id: int) -> float:
        """Get a camera property.

        Supported properties:
            - CAP_PROP_FRAME_WIDTH (3)
            - CAP_PROP_FRAME_HEIGHT (4)
            - CAP_PROP_FPS (5)

        Returns:
            Property value as float (0.0 if not available)
        """
        ...

    def set(self, prop_id: int, value: float) -> bool:
        """Set a camera property (no-op for remote cameras).

        Returns:
            Always False (property setting not supported for remote cameras)
        """
        ...

    def getBackendName(self) -> str:
        """Get the backend name.

        Returns:
            "XOQ_IROH"
        """
        ...

    def __enter__(self) -> VideoCapture: ...
    def __exit__(
        self,
        exc_type: type | None,
        exc_val: BaseException | None,
        exc_tb: object | None,
    ) -> bool: ...

# =============================================================================
# Submodules
# =============================================================================

class cv2:
    """OpenCV-compatible submodule for remote camera access.

    Example:
        # Instead of: import cv2
        from xoq import cv2

        cap = cv2.VideoCapture("server-endpoint-id")
        ret, frame = cap.read()
    """

    # Re-export VideoCapture
    VideoCapture = VideoCapture

    # OpenCV property constants
    CAP_PROP_POS_MSEC: int
    CAP_PROP_POS_FRAMES: int
    CAP_PROP_FRAME_WIDTH: int
    CAP_PROP_FRAME_HEIGHT: int
    CAP_PROP_FPS: int
    CAP_PROP_FRAME_COUNT: int

class serial:
    """pyserial-compatible submodule for remote serial port access.

    Example:
        # Instead of: import serial
        from xoq import serial

        ser = serial.Serial("server-endpoint-id")
        ser.write(b"AT\\r\\n")
        response = ser.readline()
    """

    # Re-export Serial
    Serial = Serial
