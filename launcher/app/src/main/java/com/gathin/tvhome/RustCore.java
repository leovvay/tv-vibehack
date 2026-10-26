package com.gathin.tvhome;

import android.graphics.Bitmap;

/**
 * Optional #![no_std] Rust acceleration (libtvhome_core.so) for LZ4 tile caching and zero-allocation
 * vivid colour sampling on locked AndroidBitmap buffers. Falls back to pure Java when the native
 * library is absent (e.g. JVM unit tests).
 */
final class RustCore {

    static final boolean AVAILABLE;

    static {
        boolean loaded = false;
        try {
            System.loadLibrary("tvhome_core");
            loaded = true;
        } catch (Throwable ignored) {
        }
        AVAILABLE = loaded;
    }

    private RustCore() {
    }

    /** Samples a 12x12 grid directly from the locked Bitmap buffer and returns the vivid ARGB colour. */
    static native int vividColor(Bitmap bitmap);

    /** Compresses a software Bitmap with LZ4 and writes it atomically to {@code path}. Returns 1 on success. */
    static native int writeBitmapLz4(String path, Bitmap bitmap);

    /** Decompresses an LZ4 tile file directly into the locked pixels of {@code bitmap}. Returns 1 on success. */
    static native int readBitmapLz4(String path, Bitmap bitmap);

    /** Reads the 12-byte LZ4 tile header: {@code (format << 32) | (width << 16) | height}, or 0 on error. */
    static native long readBitmapLz4Header(String path);

    /**
     * Packs all tile .lz4 files listed in {@code manifest} ({@code dir + "\n" + file0 + "\n" + ...})
     * into one contiguous {@code pack.lz4} archive for single-syscall cold-start loading.
     */
    static native int writePackLz4(String manifest);

    /** Loads {@code pack.lz4} into native memory in one read() and returns the entry count, or -1. */
    static native int loadPackLz4(String packPath);

    /** Header of entry {@code index} in the loaded pack: {@code (format << 32) | (width << 16) | height}. */
    static native long readPackHeader(int index);

    /** Decompresses entry {@code index} from the loaded pack in RAM directly into {@code bitmap}. */
    static native int readPackBitmap(int index, Bitmap bitmap);

    /** Frees the native pack and I/O scratch buffers back to the OS once batch load/save completes. */
    static native void releaseBuffers();
}
