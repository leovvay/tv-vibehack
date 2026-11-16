package com.gathin.tvhome;

import android.content.Context;
import android.graphics.Bitmap;
import android.provider.Settings;
import android.util.Log;

import java.io.File;
import java.io.IOException;

/**
 * Optional #![no_std] Rust acceleration (libtvhome_core.so) for LZ4 tile caching and zero-allocation
 * vivid colour sampling on locked AndroidBitmap buffers. Falls back to pure Java when the native
 * library is absent (e.g. JVM unit tests), switched off, or took the launcher down on its last run.
 *
 * This is the Home app, so a crash in here is a crash loop on the home screen. The library is loaded
 * only by {@link #arm}; every tile store section runs between {@link #begin} and {@link #end} with
 * {@link #LOCK} held; and a section the process died in keeps the library off until the next build.
 * Which path a process took is one line under the log tag TvHome.Native.
 */
final class RustCore {

    private static final String TAG = "TvHome.Native";
    /** {@code adb shell settings put global tvhome_native_core 0} keeps the library out of the process. */
    private static final String SWITCH = "tvhome_native_core";

    /** True once {@link #arm} has loaded the library; false sends every caller down the Java path. */
    static volatile boolean AVAILABLE;
    /** The native buffers are statics shared by every thread: one tile store section at a time. */
    static final Object LOCK = new Object();

    private static boolean armed;
    /** Present while a tile store section runs; found by {@link #arm} when a process died in one. */
    private static File guard;

    private RustCore() {
    }

    /** Decides, once per process and before any native call, whether the library is used at all. */
    static synchronized void arm(Context app) {
        if (armed) return;
        armed = true;
        String off = null;
        try {
            if (Settings.Global.getInt(app.getContentResolver(), SWITCH, 1) == 0) off = SWITCH + " is 0";
        } catch (RuntimeException ignored) {
            // A system that keeps this setting from the app: not switched off.
        }
        File mark = new File(app.getFilesDir(), "native.guard");
        if (off == null && mark.exists()) {
            // A build installed after the mark was left gets its own chance.
            if (mark.lastModified() >= new File(app.getPackageCodePath()).lastModified()) {
                off = "the last run died inside it";
            } else {
                mark.delete();
            }
        }
        if (off == null) {
            try {
                System.loadLibrary("tvhome_core");
                guard = mark;
                AVAILABLE = true;
            } catch (Throwable t) {
                off = t.toString();
            }
        }
        Log.i(TAG, off == null ? "tile store: native" : "tile store: Java (" + off + ")");
    }

    /** The start of a tile store section; call with {@link #LOCK} held. */
    static void begin() {
        try {
            guard.createNewFile();
        } catch (IOException | RuntimeException ignored) {
            // Storage that cannot hold the mark: the section runs unguarded.
        }
    }

    /** The end of a tile store section: the native buffers go back to the OS and the mark comes off. */
    static void end() {
        try {
            releaseBuffers();
        } catch (LinkageError e) {
            fail(e);
        }
        guard.delete();
    }

    /** A library that loaded but does not match this class: the Java path from here on. */
    static void fail(LinkageError e) {
        AVAILABLE = false;
        Log.w(TAG, "tile store: Java from now on (" + e + ")");
    }

    /** {@link #vividColor}, or 0 when the library cannot answer and the caller should sample in Java. */
    static int vivid(Bitmap bitmap) {
        try {
            return vividColor(bitmap);
        } catch (LinkageError e) {
            fail(e);
            return 0;
        }
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
