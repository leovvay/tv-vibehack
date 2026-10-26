package com.gathin.tvhome;

import android.content.Context;
import android.content.res.Resources;
import android.graphics.Typeface;
import android.util.TypedValue;

import java.util.concurrent.ConcurrentHashMap;

/**
 * The typeface behind one of the font_* strings in res/values/fonts.xml, for views built in code. The string
 * points at a bundled font, or, in locales Outfit can't draw (values-vi), names a system family (#833).
 */
final class UiFont {
    private static final ConcurrentHashMap<String, Typeface> CACHE = new ConcurrentHashMap<>(4);

    private UiFont() {}

    static Typeface get(Context context, int fontString) {
        Resources res = context.getResources();
        TypedValue value = new TypedValue();
        res.getValue(fontString, value, true);
        String name = String.valueOf(value.string);
        Typeface cached = CACHE.get(name);
        if (cached != null) return cached;
        Typeface loaded = name.startsWith("res/")
                ? res.getFont(value.resourceId)
                : Typeface.create(name, Typeface.NORMAL);
        if (loaded != null) CACHE.put(name, loaded);
        return loaded;
    }

    /** Pre-warms bundled Typefaces on an idle core during Application.onCreate before layout inflation begins. */
    static void warm(Context context) {
        try {
            get(context, R.string.font_light);
            get(context, R.string.font_medium);
            get(context, R.string.font_regular);
        } catch (Throwable ignored) {
        }
    }
}
