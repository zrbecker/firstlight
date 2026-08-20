# Touptek SDK

This directory is where the vendor SDK goes. Nothing in it is redistributable,
so it is git-ignored and the `sdk` feature is off by default.

1. Download the "Windows/macOS/Linux SDK" archive from
   <https://www.touptek-astro.com/download/> (SVBONY ship the same SDK for the
   SV305C Pro; either vendor's copy works).
2. Unpack it here, so that this directory contains:

   ```
   vendor/inc/toupcam.h
   vendor/mac/libtoupcam.dylib
   vendor/linux/x64/libtoupcam.so
   vendor/win/x64/toupcam.lib
   ```

   Or leave it wherever you unpacked it and set
   `FIRSTLIGHT_TOUPTEK_SDK_DIR=/path/to/sdk`.
3. Build with the feature on:

   ```sh
   cargo build --features touptek        # from firstlight-cli / firstlight-view
   cargo build -p firstlight-touptek --features sdk
   ```

At runtime the loader must be able to find `libtoupcam.dylib`. The build script
adds an rpath pointing at the SDK directory and at `@loader_path`, so copying
the dylib next to the binary (or into `Contents/Frameworks` of an app bundle)
also works.

## Linux

The SDK needs a udev rule before a non-root user can open a camera; the archive
ships one in `linux/99-toupcam.rules`. Copy it to `/etc/udev/rules.d/` and
`sudo udevadm control --reload`.
