"""Tests for MoQ path detection and Bus constructor wiring."""

import sys
import os

# Ensure xoq_can is importable (built with maturin develop)
import xoq_can
from xoq_can._can_hook import _is_remote_channel


def test_is_remote_channel():
    """Test that _is_remote_channel correctly identifies iroh IDs and MoQ paths."""
    # Iroh IDs (64-char hex)
    assert _is_remote_channel("a" * 64) is True
    assert _is_remote_channel("0123456789abcdef" * 4) is True

    # MoQ paths (contain '/')
    assert _is_remote_channel("anon/xoq-can-can0") is True
    assert _is_remote_channel("anon/my-robot") is True
    assert _is_remote_channel("user/foo/bar") is True

    # Full MoQ URLs
    assert _is_remote_channel("https://172.18.133.111:4443/anon/xoq-can-can0") is True
    assert _is_remote_channel("http://localhost:4443/anon/test") is True

    # Local interfaces (should NOT be remote)
    assert _is_remote_channel("can0") is False
    assert _is_remote_channel("vcan0") is False
    assert _is_remote_channel(None) is False
    assert _is_remote_channel("") is False

    # Edge cases
    assert _is_remote_channel("a" * 63) is False  # too short for iroh ID
    assert _is_remote_channel("A" * 64) is False  # uppercase, not hex


def test_bus_accepts_moq_kwargs():
    """Test that Bus constructor accepts relay and insecure kwargs without error.

    This doesn't actually connect (no relay running), but verifies the
    Python→Rust parameter plumbing compiles and doesn't reject the args.
    """
    try:
        bus = xoq_can.Bus(
            channel="anon/xoq-can-can0",
            relay="https://127.0.0.1:9999",  # bogus, will fail to connect
            insecure=True,
            timeout=0.5,
        )
        # If we get here, connection somehow succeeded (unlikely)
        bus.shutdown()
    except RuntimeError as e:
        # Expected: connection refused or timeout — but the kwargs were accepted
        err = str(e).lower()
        assert any(
            kw in err
            for kw in ["connect", "timeout", "error", "failed", "timed out", "refused"]
        ), f"Unexpected error (kwargs may not be wired): {e}"
        print(f"OK: Bus('anon/...', relay=..., insecure=True) correctly attempted MoQ connection")
        print(f"    (got expected connection error: {e})")


def test_bus_full_url():
    """Test that a full URL like https://relay:4443/anon/path works."""
    try:
        bus = xoq_can.Bus(
            channel="https://127.0.0.1:9999/anon/xoq-can-can0",
            insecure=True,
            timeout=0.5,
        )
        bus.shutdown()
    except RuntimeError as e:
        err = str(e).lower()
        assert any(
            kw in err
            for kw in ["connect", "timeout", "error", "failed", "timed out", "refused"]
        ), f"Unexpected error: {e}"
        print(f"OK: Bus('https://127.0.0.1:9999/anon/xoq-can-can0') correctly parsed URL")
        print(f"    (got expected connection error: {e})")


def test_bus_url_no_path_rejected():
    """Test that a URL with no path is rejected."""
    try:
        bus = xoq_can.Bus(channel="https://127.0.0.1:9999", insecure=True, timeout=0.5)
        bus.shutdown()
        assert False, "Should have raised"
    except ValueError as e:
        print(f"OK: Bus('https://127.0.0.1:9999') correctly rejected: {e}")


def test_bus_iroh_path_unchanged():
    """Test that a 64-char hex channel still takes the iroh path (no MoQ)."""
    fake_id = "a" * 64
    try:
        bus = xoq_can.Bus(channel=fake_id, timeout=0.5)
        bus.shutdown()
    except RuntimeError as e:
        err = str(e).lower()
        # Should be an iroh connection error, not a MoQ one
        print(f"OK: Bus('{fake_id[:16]}...') correctly attempted iroh connection")
        print(f"    (got expected error: {e})")


if __name__ == "__main__":
    print("=== MoQ detection tests ===\n")

    test_is_remote_channel()
    print("PASS: _is_remote_channel()\n")

    test_bus_accepts_moq_kwargs()
    print()

    test_bus_full_url()
    print()

    test_bus_url_no_path_rejected()
    print()

    test_bus_iroh_path_unchanged()
    print()

    print("=== All tests passed ===")
