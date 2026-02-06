"""Hardware connectivity tests for CI.

These tests validate that always-running hardware servers (serial, camera, CAN)
are reachable from GitHub Actions CI runners via Iroh relay NAT traversal.

Each test class is gated by an environment variable containing the server's
Iroh endpoint ID. Tests skip if the variable is not set.

Environment variables:
    XOQ_SERIAL_SERVER_ID: Iroh endpoint ID of the serial server (SO100)
    XOQ_CAMERA_SERVER_ID: Iroh endpoint ID of the camera server
    XOQ_CAN_SERVER_ID:    Iroh endpoint ID of the CAN server (openarm)
    XOQ_AUDIO_SERVER_ID:  Iroh endpoint ID of the audio server
"""

import os

import pytest

SERIAL_SERVER_ID = os.environ.get("XOQ_SERIAL_SERVER_ID")
CAMERA_SERVER_ID = os.environ.get("XOQ_CAMERA_SERVER_ID")
CAN_SERVER_ID = os.environ.get("XOQ_CAN_SERVER_ID")
AUDIO_SERVER_ID = os.environ.get("XOQ_AUDIO_SERVER_ID")


@pytest.mark.skipif(not SERIAL_SERVER_ID, reason="XOQ_SERIAL_SERVER_ID not set")
class TestSerialConnectivity:
    """Test serial server reachability via Iroh relay."""

    @pytest.mark.timeout(60)
    def test_serial_connect(self):
        import xoq_serial

        ser = xoq_serial.Serial(SERIAL_SERVER_ID, timeout=10.0)
        assert ser.is_open
        ser.close()

    @pytest.mark.timeout(60)
    def test_serial_read(self):
        import xoq_serial

        ser = xoq_serial.Serial(SERIAL_SERVER_ID, timeout=10.0)
        try:
            data = ser.read(1)
            # Success: no exception raised. Data may be empty on timeout.
            assert isinstance(data, bytes)
        finally:
            ser.close()


@pytest.mark.skipif(not CAMERA_SERVER_ID, reason="XOQ_CAMERA_SERVER_ID not set")
class TestCameraConnectivity:
    """Test camera server reachability via Iroh relay."""

    @pytest.mark.timeout(60)
    def test_camera_connect(self):
        import xoq_cv2

        cap = xoq_cv2.VideoCapture(CAMERA_SERVER_ID, "iroh")
        assert cap.isOpened()
        cap.release()

    @pytest.mark.timeout(60)
    def test_camera_read_frame(self):
        import xoq_cv2

        cap = xoq_cv2.VideoCapture(CAMERA_SERVER_ID, "iroh")
        try:
            ret, frame = cap.read()
            assert ret is True
            assert frame is not None
            # Frame should be a 3D array: (height, width, channels)
            assert len(frame.shape) == 3
            assert frame.shape[2] == 3  # BGR
        finally:
            cap.release()


@pytest.mark.skipif(not CAN_SERVER_ID, reason="XOQ_CAN_SERVER_ID not set")
class TestCanConnectivity:
    """Test CAN server reachability via Iroh relay."""

    @pytest.mark.timeout(60)
    def test_can_connect(self):
        import xoq_can

        bus = xoq_can.Bus(channel=CAN_SERVER_ID, timeout=10.0)
        assert bus.channel_info is not None
        bus.shutdown()

    @pytest.mark.timeout(60)
    def test_can_recv(self):
        import xoq_can

        bus = xoq_can.Bus(channel=CAN_SERVER_ID, timeout=10.0)
        try:
            # recv returns None on timeout, which is fine — we just want no exception
            msg = bus.recv(timeout=5.0)
            assert msg is None or isinstance(msg, xoq_can.Message)
        finally:
            bus.shutdown()


@pytest.mark.skipif(not AUDIO_SERVER_ID, reason="XOQ_AUDIO_SERVER_ID not set")
class TestAudioConnectivity:
    """Test audio server reachability via Iroh relay."""

    @pytest.mark.timeout(60)
    def test_audio_connect(self):
        import xoq_sounddevice

        stream = xoq_sounddevice.Stream(AUDIO_SERVER_ID, samplerate=48000, channels=1)
        assert stream.is_active
        stream.stop()

    @pytest.mark.timeout(60)
    def test_audio_read(self):
        import xoq_sounddevice
        import numpy as np

        stream = xoq_sounddevice.Stream(AUDIO_SERVER_ID, samplerate=48000, channels=1)
        try:
            # Read 960 frames (20ms @ 48kHz)
            data = stream.read(960)
            assert isinstance(data, np.ndarray)
            assert data.shape == (960, 1)
        finally:
            stream.stop()
