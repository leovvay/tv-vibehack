package com.gathin.tvhome;

import android.content.Context;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;

import java.io.BufferedInputStream;
import java.io.BufferedOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.nio.Buffer;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;

/**
 * The tiles of the last load, kept on disk so a cold start can show them without opening any other app's APK.
 * A small index plus one file of raw pixels per tile in the cache directory, so reading a tile back is one
 * copy and no decoding. Read by {@link TvHomeApp}'s preloader, written by {@link AppAdapter}'s bake worker.
 *
 * A missing, old or damaged store is ignored, never an error: the normal load builds the tiles and saves them again.
 */
final class TileStore {

    private static final int MAGIC = 0x54494C45;   // "TILE"
    private static final int VERSION = 4;
    private static final String INDEX = "index";
    /** A tile file: a 12-byte header, then the pixels as the Bitmap holds them. */
    private static final int RAW_MAGIC = 0x54524157;   // "TRAW"
    private static final int RAW_HEADER = 12;

    /** One saved tile: the banner, or the icon of an app without one. */
    static final class Tile {
        final String key;
        final String label;
        /** The package's lastUpdateTime when the tile was built. */
        final long stamp;
        final int ambient;
        final boolean banner;
        final boolean leanback;
        final String file;
        /** Decoded by {@link #read()}; null when the image could not be read. */
        Bitmap image;

        Tile(String key, String label, long stamp, int ambient, boolean banner, boolean leanback, String file) {
            this.key = key;
            this.label = label;
            this.stamp = stamp;
            this.ambient = ambient;
            this.banner = banner;
            this.leanback = leanback;
            this.file = file;
        }
    }

    /** What was saved, and for which locale and banner size. */
    static final class Contents {
        final String locale;
        final int width, height;
        final List<Tile> tiles;

        Contents(String locale, int width, int height, List<Tile> tiles) {
            this.locale = locale;
            this.width = width;
            this.height = height;
            this.tiles = tiles;
        }
    }

    private final File dir;
    private int written;

    TileStore(Context app) {
        dir = new File(app.getCacheDir(), "tiles");
    }

    /**
     * The saved tiles with their images decoded straight to GPU memory, or null when there is nothing usable.
     * {@code build} is the launcher's own lastUpdateTime: tiles saved by another build of the launcher may have been
     * drawn, coloured or labelled differently, so they are not used.
     */
    Contents read(long build) {
        Contents c;
        try (DataInputStream in = new DataInputStream(new BufferedInputStream(new FileInputStream(new File(dir, INDEX))))) {
            if (in.readInt() != MAGIC || in.readInt() != VERSION || in.readLong() != build) return null;
            String locale = in.readUTF();
            int w = in.readInt(), h = in.readInt(), n = in.readInt();
            if (n < 0 || n > 4096) return null;
            List<Tile> tiles = new ArrayList<>(n);
            for (int i = 0; i < n; i++) {
                Tile t = new Tile(in.readUTF(), in.readUTF(), in.readLong(), in.readInt(),
                        in.readBoolean(), in.readBoolean(), in.readUTF());
                if (t.file.indexOf('/') >= 0) return null;
                tiles.add(t);
            }
            c = new Contents(locale, w, h, tiles);
        } catch (IOException | RuntimeException e) {
            return null;
        }
        // The tiles come back as RGB_565 / ARGB_8888 software bitmaps, so Frame 1 never blocks on a synchronous
        // RenderProxy::allocateHardwareBitmap upload; AppAdapter moves banners to HARDWARE off the UI thread
        // right after Frame 1.
        BitmapFactory.Options opts = null;
        ByteBuffer[] scratch = new ByteBuffer[1];
        for (Tile t : c.tiles) {
            if (t.file.endsWith(".raw")) {
                t.image = readRaw(new File(dir, t.file), scratch);
                continue;
            }
            try {
                if (opts == null) {
                    opts = new BitmapFactory.Options();
                    opts.inScaled = false;
                }
                t.image = BitmapFactory.decodeFile(new File(dir, t.file).getPath(), opts);
            } catch (RuntimeException ignored) {
            }
        }
        return c;
    }

    /** Saves one tile image and returns its file name, or null when it could not be written. */
    String writeImage(Bitmap image) {
        // Banners live in GPU memory unless passed from AppLoader.Built as software bitmaps.
        Bitmap soft = image.getConfig() == Bitmap.Config.HARDWARE ? image.copy(Bitmap.Config.ARGB_8888, false) : image;
        if (soft == null) return null;
        try {
            if (!dir.isDirectory() && !dir.mkdirs()) return null;
            // The pixels as they are, so reading them back is one copy and no decoding.
            String name;
            do {
                name = Long.toHexString(System.currentTimeMillis()) + "-" + (written++) + ".raw";
            } while (new File(dir, name).exists());
            if (writeRaw(new File(dir, name), soft)) return name;
            // A pixel format the raw file does not carry: standard PNG compression.
            do {
                name = Long.toHexString(System.currentTimeMillis()) + "-" + (written++) + ".png";
            } while (new File(dir, name).exists());
            File tmp = new File(dir, name + ".tmp");
            try (OutputStream out = new BufferedOutputStream(new FileOutputStream(tmp))) {
                if (!soft.compress(Bitmap.CompressFormat.PNG, 100, out)) return null;
            }
            return tmp.renameTo(new File(dir, name)) ? name : null;
        } catch (IOException | RuntimeException e) {
            return null;
        } finally {
            if (soft != image) soft.recycle();
        }
    }

    /** Bytes per pixel of the two formats a raw tile carries, or 0 for any other. */
    private static int rawFormat(Bitmap.Config config) {
        return config == Bitmap.Config.RGB_565 ? 2 : config == Bitmap.Config.ARGB_8888 ? 4 : 0;
    }

    private static boolean writeRaw(File file, Bitmap soft) throws IOException {
        int format = rawFormat(soft.getConfig()), w = soft.getWidth(), h = soft.getHeight();
        if (format == 0 || w > 4096 || h > 4096 || soft.getRowBytes() != w * format) return false;
        ByteBuffer buf = ByteBuffer.allocate(RAW_HEADER + w * h * format);
        buf.putInt(RAW_MAGIC).putShort((short) w).putShort((short) h).putInt(format);
        soft.copyPixelsToBuffer(buf);
        File tmp = new File(file.getPath() + ".tmp");
        try (FileOutputStream out = new FileOutputStream(tmp)) {
            out.write(buf.array(), 0, buf.position());
        }
        return tmp.renameTo(file);
    }

    /** The tile {@link #writeRaw} saved, or null when the file is not one. {@code scratch} is reused between tiles. */
    private static Bitmap readRaw(File file, ByteBuffer[] scratch) {
        try (FileInputStream in = new FileInputStream(file)) {
            FileChannel ch = in.getChannel();
            ByteBuffer head = ByteBuffer.allocate(RAW_HEADER);
            while (head.hasRemaining()) if (ch.read(head) < 0) return null;
            // Through Buffer: ByteBuffer's own flip/clear/limit only exist on newer Android than this may run on.
            ((Buffer) head).flip();
            if (head.getInt() != RAW_MAGIC) return null;
            int w = head.getShort() & 0xFFFF, h = head.getShort() & 0xFFFF, format = head.getInt();
            Bitmap.Config config = format == 2 ? Bitmap.Config.RGB_565 : format == 4 ? Bitmap.Config.ARGB_8888 : null;
            if (config == null || w == 0 || h == 0 || w > 4096 || h > 4096) return null;
            int bytes = w * h * format;
            if (ch.size() != (long) RAW_HEADER + bytes) return null;
            ByteBuffer px = scratch[0];
            if (px == null || px.capacity() < bytes) scratch[0] = px = ByteBuffer.allocateDirect(bytes);
            ((Buffer) px).clear();
            ((Buffer) px).limit(bytes);
            while (px.hasRemaining()) if (ch.read(px) < 0) return null;
            ((Buffer) px).flip();
            Bitmap bmp = Bitmap.createBitmap(w, h, config);
            bmp.copyPixelsFromBuffer(px);
            if (format == 4) bmp.setHasAlpha(true);
            return bmp;
        } catch (IOException | RuntimeException e) {
            return null;
        }
    }

    /**
     * Replaces the index with these tiles and deletes every file it no longer names. The images are written
     * before the index and the index is swapped in with a rename, so a crash leaves the old store or the new one.
     */
    void writeIndex(long build, String locale, int w, int h, List<Tile> tiles) {
        try {
            if (!dir.isDirectory() && !dir.mkdirs()) return;
            File tmp = new File(dir, INDEX + ".tmp");
            try (DataOutputStream out = new DataOutputStream(new BufferedOutputStream(new FileOutputStream(tmp)))) {
                out.writeInt(MAGIC);
                out.writeInt(VERSION);
                out.writeLong(build);
                out.writeUTF(locale);
                out.writeInt(w);
                out.writeInt(h);
                out.writeInt(tiles.size());
                for (Tile t : tiles) {
                    out.writeUTF(t.key);
                    out.writeUTF(t.label);
                    out.writeLong(t.stamp);
                    out.writeInt(t.ambient);
                    out.writeBoolean(t.banner);
                    out.writeBoolean(t.leanback);
                    out.writeUTF(t.file);
                }
            }
            if (!tmp.renameTo(new File(dir, INDEX))) return;
            Set<String> keep = new HashSet<>();
            keep.add(INDEX);
            for (Tile t : tiles) keep.add(t.file);
            File[] files = dir.listFiles();
            if (files == null) return;
            for (File f : files) {
                if (!keep.contains(f.getName())) f.delete();
            }
        } catch (IOException | RuntimeException ignored) {
        }
    }
}
