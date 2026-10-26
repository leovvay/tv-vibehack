package com.gathin.tvhome;

import android.app.Application;
import android.content.Context;
import android.os.Process;

/**
 * Multi-core cold-start preloader for Cortex-A53/A55 TVs: while ActivityThread sets up the window and
 * inflates the root decor on Core 0 (~200 ms between makeApplication and MainActivity.onResume), an idle
 * core warms the variable-font Typefaces, loads libtvhome_core.so + pack.lz4 into RGB_565 buffers, and
 * reads Config + Order off the UI thread.
 */
public final class TvHomeApp extends Application {

    private static final Object LOCK = new Object();
    private static boolean preloadDone;
    private static TileStore.Contents preloadedTiles;
    private static Config preloadedConfig;
    private static Order preloadedOrder;

    @Override
    public void onCreate() {
        super.onCreate();
        final Context app = this;
        Thread t = new Thread(() -> {
            Process.setThreadPriority(Process.THREAD_PRIORITY_DISPLAY);
            UiFont.warm(app);
            TileStore.Contents tiles = null;
            Config cfg = null;
            Order order = null;
            try {
                tiles = new TileStore(app).read(AppAdapter.selfStamp(app, app.getPackageManager()));
            } catch (Throwable ignored) {
            }
            try {
                cfg = Config.load(app);
            } catch (Throwable ignored) {
            }
            try {
                order = Order.load(Order.file(app));
            } catch (Throwable ignored) {
            }
            synchronized (LOCK) {
                preloadedTiles = tiles;
                preloadedConfig = cfg;
                preloadedOrder = order;
                preloadDone = true;
                LOCK.notifyAll();
            }
        }, "tvhome-preload");
        t.setDaemon(true);
        t.start();
    }

    private static void awaitPreload() {
        synchronized (LOCK) {
            long deadline = System.currentTimeMillis() + 150L;
            while (!preloadDone) {
                long rem = deadline - System.currentTimeMillis();
                if (rem <= 0) break;
                try {
                    LOCK.wait(rem);
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                    break;
                }
            }
        }
    }

    static Config takeConfig(Context ctx) {
        awaitPreload();
        Config c;
        synchronized (LOCK) {
            c = preloadedConfig;
            preloadedConfig = null;
        }
        return c != null && c.locale.isEmpty() ? c : Config.load(ctx);
    }

    static Order takeOrder(Context ctx) {
        awaitPreload();
        Order o;
        synchronized (LOCK) {
            o = preloadedOrder;
            preloadedOrder = null;
        }
        return o != null ? o : Order.load(Order.file(ctx));
    }

    static TileStore.Contents takeTiles(Context ctx, TileStore store) {
        awaitPreload();
        TileStore.Contents t;
        synchronized (LOCK) {
            t = preloadedTiles;
            preloadedTiles = null;
        }
        return t != null ? t : store.read(AppAdapter.selfStamp(ctx, ctx.getPackageManager()));
    }
}
