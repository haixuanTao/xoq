"""Startup hook that patches ``import can`` to include xoq remote CAN buses.

Installed via ``xoq_can_hook.pth`` so it runs automatically at interpreter
startup.  After the hook fires once it removes itself from
``sys.meta_path`` — zero overhead for subsequent imports.

When python-can is installed, ``import can`` loads the real package and patches
Bus with a metaclass dispatcher.  When python-can is NOT installed,
``import can`` creates a synthetic module backed entirely by xoq_can.
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


def _is_remote_channel(channel):
    """Return True if *channel* looks like a remote CAN bus identifier.

    Matches iroh node IDs (64-char hex), MoQ paths (contains '/'),
    and full MoQ URLs (https://...).
    """
    s = str(channel) if channel is not None else ""
    return bool(_IROH_ID_RE.match(s)) or '/' in s or s.startswith("https://") or s.startswith("http://")


class _MissingPythonCan:
    """Placeholder for python-can's Bus when python-can is not installed."""

    def __init__(self, *args, **kwargs):
        raise ImportError(
            "python-can is not installed. Install it with: pip install python-can\n"
            "Only remote CAN buses (64-char hex iroh IDs) work without python-can."
        )


class _XoqBusType(type):
    """Metaclass: dispatches Bus() construction based on channel identifier."""

    def __call__(cls, *args, **kwargs):
        channel = args[0] if args else kwargs.get("channel")

        if _is_remote_channel(channel):
            # Strip args that xoq doesn't need but python-can passes
            kwargs.pop("interface", None)
            # Keep relay and insecure for MoQ paths, pass through to xoq_can.Bus
            if args:
                return cls._xoq(*args, **kwargs)
            return cls._xoq(**kwargs)

        # Strip MoQ-specific kwargs before passing to python-can
        kwargs.pop("relay", None)
        kwargs.pop("insecure", None)
        return cls._real(*args, **kwargs)

    def __instancecheck__(cls, instance):
        return type.__instancecheck__(cls, instance) or isinstance(
            instance, (cls._real, cls._xoq)
        )


class _XoqBus(metaclass=_XoqBusType):
    _real = _MissingPythonCan
    _xoq = object


def _patch_can(mod):
    """Patch can.Bus and can.interface.Bus with xoq-aware wrapper."""
    try:
        import xoq_can as _xoq

        real_bus = getattr(mod, "Bus", _MissingPythonCan)
        if not isinstance(real_bus, _XoqBusType):
            _XoqBus._real = real_bus
            _XoqBus._xoq = _xoq.Bus
            _XoqBus.__name__ = "Bus"
            _XoqBus.__qualname__ = "Bus"
            mod.Bus = _XoqBus

        # Also patch can.interface.Bus (lerobot uses this path)
        iface = getattr(mod, "interface", None)
        if iface is not None and hasattr(iface, "Bus"):
            if not isinstance(iface.Bus, _XoqBusType):
                iface.Bus = _XoqBus
        elif iface is None:
            # Create synthetic can.interface submodule
            iface = types.ModuleType("can.interface")
            iface.Bus = _XoqBus
            mod.interface = iface
            sys.modules["can.interface"] = iface

        # Ensure can.Message exists (from xoq_can if not from python-can)
        if not hasattr(mod, "Message"):
            mod.Message = _xoq.Message
    except ImportError:
        pass


def _make_synthetic_can():
    """Create a synthetic ``can`` module backed entirely by xoq_can."""
    import xoq_can as _xoq

    mod = types.ModuleType("can")
    mod.__package__ = "can"
    mod.__path__ = []  # make it a package so `from can import ...` works

    # Set up xoq Bus with dispatcher
    _XoqBus._real = _MissingPythonCan
    _XoqBus._xoq = _xoq.Bus
    _XoqBus.__name__ = "Bus"
    _XoqBus.__qualname__ = "Bus"
    mod.Bus = _XoqBus
    mod.Message = _xoq.Message

    # Create can.interface submodule
    iface = types.ModuleType("can.interface")
    iface.Bus = _XoqBus
    mod.interface = iface
    sys.modules["can.interface"] = iface

    # Copy constants from xoq_can
    for name in dir(_xoq):
        if name.startswith("INTERFACE_"):
            setattr(mod, name, getattr(_xoq, name))

    return mod


class _CanFinder(importlib.abc.MetaPathFinder):
    """One-shot meta-path finder that intercepts ``import can``."""

    _resolving = False  # guard against recursive find_spec calls

    def find_spec(self, fullname, path, target=None):
        if fullname != "can" or self._resolving:
            return None

        # Guard against recursion while we call find_spec for the real package
        self._resolving = True
        try:
            spec = importlib.util.find_spec("can")
        finally:
            self._resolving = False

        if spec is not None:
            # Wrap the loader to patch after loading, then remove ourselves
            original_loader = spec.loader
            spec.loader = _PatchingLoader(original_loader, self)
            return spec

        # python-can not installed — provide synthetic module from xoq_can
        return importlib.machinery.ModuleSpec(
            "can",
            _SyntheticCanLoader(self),
            origin="xoq_can",
        )


class _PatchingLoader:
    """Loader wrapper that patches can after the real loader finishes."""

    def __init__(self, original, finder):
        self._original = original
        self._finder = finder

    def create_module(self, spec):
        if hasattr(self._original, "create_module"):
            return self._original.create_module(spec)
        return None

    def exec_module(self, module):
        self._original.exec_module(module)
        _patch_can(module)
        # Remove the finder now that we've successfully loaded and patched
        sys.meta_path[:] = [f for f in sys.meta_path if f is not self._finder]


class _SyntheticCanLoader:
    """Loader that creates a synthetic can module backed by xoq_can."""

    def __init__(self, finder):
        self._finder = finder

    def create_module(self, spec):
        return _make_synthetic_can()

    def exec_module(self, module):
        # Remove the finder now that we've provided the synthetic module
        sys.meta_path[:] = [f for f in sys.meta_path if f is not self._finder]


def install():
    """Insert the can import hook (idempotent, guards against re-entry)."""
    # Already imported — patch in place
    if "can" in sys.modules:
        _patch_can(sys.modules["can"])
        return

    # xoq_can not available — nothing to do
    try:
        import xoq_can  # noqa: F401
    except ImportError:
        return

    # Don't double-install
    if any(isinstance(f, _CanFinder) for f in sys.meta_path):
        return

    sys.meta_path.insert(0, _CanFinder())
