"""Startup hook that patches ``import serial`` to include xoq remote serial ports.

Installed via ``xoq_serial_hook.pth`` so it runs automatically at interpreter
startup.  After the hook fires once it removes itself from
``sys.meta_path`` — zero overhead for subsequent imports.

When pyserial is installed, ``import serial`` loads the real package and patches
Serial with a metaclass dispatcher.  When pyserial is NOT installed,
``import serial`` creates a synthetic module backed entirely by xoq_serial.
"""

import importlib
import importlib.abc
import importlib.machinery
import importlib.util
import re
import sys
import types


# Pattern for iroh node IDs (64-char hex-encoded ed25519 public keys)
_IROH_ID_RE = re.compile(r"^[a-f0-9]{64}$")


def _is_remote_port(port):
    """Return True if *port* looks like a remote serial port identifier."""
    s = str(port) if port is not None else ""
    return bool(_IROH_ID_RE.match(s))


class _MissingPySerial:
    """Placeholder for pyserial's Serial when pyserial is not installed."""

    def __init__(self, *args, **kwargs):
        raise ImportError(
            "pyserial is not installed. Install it with: pip install pyserial\n"
            "Only remote serial ports (64-char hex iroh IDs) work without pyserial."
        )


class _XoqSerialType(type):
    """Metaclass: dispatches Serial() construction based on port identifier."""

    def __call__(cls, *args, **kwargs):
        port = args[0] if args else kwargs.get("port")

        if _is_remote_port(port):
            # Only pass port and timeout to xoq; strip pyserial-specific kwargs
            xoq_kwargs = {}
            if "timeout" in kwargs:
                xoq_kwargs["timeout"] = kwargs["timeout"]
            if args:
                return cls._xoq(args[0], **xoq_kwargs)
            return cls._xoq(port=port, **xoq_kwargs)

        return cls._real(*args, **kwargs)

    def __instancecheck__(cls, instance):
        return type.__instancecheck__(cls, instance) or isinstance(
            instance, (cls._real, cls._xoq)
        )


class _XoqSerial(metaclass=_XoqSerialType):
    _real = _MissingPySerial
    _xoq = object


class _SerialException(Exception):
    """Fallback SerialException when pyserial is not installed."""
    pass


def _patch_serial(mod):
    """Patch serial.Serial with xoq-aware wrapper."""
    try:
        import xoq_serial as _xoq

        real_serial = getattr(mod, "Serial", _MissingPySerial)
        if not isinstance(real_serial, _XoqSerialType):
            _XoqSerial._real = real_serial
            _XoqSerial._xoq = _xoq.Serial
            _XoqSerial.__name__ = "Serial"
            _XoqSerial.__qualname__ = "Serial"
            mod.Serial = _XoqSerial

        # Ensure SerialException exists
        if not hasattr(mod, "SerialException"):
            mod.SerialException = _SerialException
    except ImportError:
        pass


def _make_synthetic_serial():
    """Create a synthetic ``serial`` module backed entirely by xoq_serial."""
    import xoq_serial as _xoq

    mod = types.ModuleType("serial")
    mod.__package__ = "serial"
    mod.__path__ = []  # make it a package so `from serial import ...` works

    # Set up xoq Serial with dispatcher
    _XoqSerial._real = _MissingPySerial
    _XoqSerial._xoq = _xoq.Serial
    _XoqSerial.__name__ = "Serial"
    _XoqSerial.__qualname__ = "Serial"
    mod.Serial = _XoqSerial

    # Provide common serial constants and exceptions
    mod.SerialException = _SerialException

    # Copy constants from xoq_serial (PARITY_*, STOPBITS_*, etc.)
    for name in dir(_xoq):
        if name.startswith(("PARITY_", "STOPBITS_", "FIVEBITS", "SIXBITS", "SEVENBITS", "EIGHTBITS")):
            setattr(mod, name, getattr(_xoq, name))

    return mod


class _SerialFinder(importlib.abc.MetaPathFinder):
    """One-shot meta-path finder that intercepts ``import serial``."""

    def find_spec(self, fullname, path, target=None):
        if fullname != "serial":
            return None

        # Remove ourselves to avoid recursion
        sys.meta_path[:] = [f for f in sys.meta_path if f is not self]

        # Try the real pyserial first
        spec = importlib.util.find_spec("serial")
        if spec is not None and spec.loader is not None:
            # Wrap the loader to patch after loading
            original_loader = spec.loader
            spec.loader = _PatchingLoader(original_loader)
            return spec

        if spec is not None:
            # Namespace package (loader is None) — let it load normally
            # and register a post-import hook to patch it
            _PostImportPatcher.install()
            return spec

        # pyserial not installed — provide synthetic module from xoq_serial
        return importlib.machinery.ModuleSpec(
            "serial",
            _SyntheticSerialLoader(),
            origin="xoq_serial",
        )


class _PostImportPatcher(importlib.abc.MetaPathFinder):
    """Patches serial after it finishes loading (for namespace packages)."""

    @classmethod
    def install(cls):
        if not any(isinstance(f, cls) for f in sys.meta_path):
            sys.meta_path.append(cls())

    def find_module(self, fullname, path=None):
        if fullname.startswith("serial") and "serial" in sys.modules:
            mod = sys.modules["serial"]
            if hasattr(mod, "Serial") and not isinstance(getattr(mod, "Serial", None), _XoqSerialType):
                _patch_serial(mod)
            sys.meta_path[:] = [f for f in sys.meta_path if f is not self]
        return None


class _PatchingLoader:
    """Loader wrapper that patches serial after the real loader finishes."""

    def __init__(self, original):
        self._original = original

    def create_module(self, spec):
        if hasattr(self._original, "create_module"):
            return self._original.create_module(spec)
        return None

    def exec_module(self, module):
        self._original.exec_module(module)
        _patch_serial(module)


class _SyntheticSerialLoader:
    """Loader that creates a synthetic serial module backed by xoq_serial."""

    def create_module(self, spec):
        return _make_synthetic_serial()

    def exec_module(self, module):
        pass  # already populated in create_module


def install():
    """Insert the serial import hook (idempotent, guards against re-entry)."""
    # Already imported — patch in place
    if "serial" in sys.modules:
        _patch_serial(sys.modules["serial"])
        return

    # xoq_serial not available — nothing to do
    try:
        import xoq_serial  # noqa: F401
    except ImportError:
        return

    # Don't double-install
    if any(isinstance(f, _SerialFinder) for f in sys.meta_path):
        return

    sys.meta_path.insert(0, _SerialFinder())
