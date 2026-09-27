package dev.birb.wgpu.rust

import com.google.gson.JsonParser

/**
 * The renderer's own settings, read from the JVM side.
 *
 * [WgpuNative.getSettings] hands back the same JSON the options screen edits and sends back, so
 * this is a JNI call and a parse - fine for the handful of switches the backend has to know about
 * itself, and not something to do per frame. The screen reads the same document once when it
 * builds its widgets.
 */
object RendererSettings {

    /** The value of a bool setting, or null when the renderer has no such setting (yet). */
    @JvmStatic
    fun bool(name: String): Boolean? = runCatching {
        JsonParser.parseString(WgpuNative.getSettings())
            .asJsonObject
            .getAsJsonObject(name)
            ?.getAsJsonPrimitive("value")
            ?.asBoolean
    }.getOrNull()

    /** The value of an int setting, or null when the renderer has no such setting (yet). */
    @JvmStatic
    fun int(name: String): Int? = runCatching {
        JsonParser.parseString(WgpuNative.getSettings())
            .asJsonObject
            .getAsJsonObject(name)
            ?.getAsJsonPrimitive("value")
            ?.asInt
    }.getOrNull()
}
