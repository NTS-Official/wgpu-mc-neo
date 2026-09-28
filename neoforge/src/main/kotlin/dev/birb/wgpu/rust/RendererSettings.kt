package dev.birb.wgpu.rust

import com.google.gson.JsonElement
import com.google.gson.JsonParser

/**
 * The renderer's own settings, read from the JVM side.
 *
 * [WgpuNative.getSettings] hands back the same JSON the options screen edits and sends back, so
 * this is a JNI call and a parse - which is why the readers below are the *cached* ones: a setting
 * read once a frame would be a JNI call and a JSON parse once a frame, and one of the callers here
 * (`wgpu_mc$firesAreFrozen`, from the atlas animation) runs for every animated sprite in the block
 * atlas on every client tick.
 *
 * The cache is why a setting appears to take up to [CACHE_NANOS] to take effect from here. That is
 * invisible for the switches this is used for - the options screen is open, not a frame being
 * drawn - and `sendSettings` still applies the value to the renderer immediately; this is only how
 * soon the *JVM* side notices it.
 */
object RendererSettings {

    /** How long a read value is kept before the renderer is asked again. */
    private const val CACHE_NANOS = 1_000_000_000L

    private val cached = HashMap<String, JsonElement?>()

    @Volatile
    private var cachedAt = 0L

    /**
     * One setting's entry in the renderer's document, or null when there is no such setting.
     *
     * Cached, and refreshed as a whole rather than per name: the callers below want a handful of
     * settings out of the same document, and one parse answering all of them is the point.
     *
     * Synchronized because the callers are not all on one thread: the atlas animation is ticked from
     * the client thread, while `RustChunkBake` polls from whichever thread is meshing a section.
     */
    @Synchronized
    private fun entry(name: String): JsonElement? {
        val now = System.nanoTime()

        if (cachedAt == 0L || now - cachedAt >= CACHE_NANOS) {
            val document = runCatching {
                JsonParser.parseString(WgpuNative.getSettings()).asJsonObject
            }.getOrNull()

            cached.clear()

            // A parse that failed leaves the cache empty, so every reader below answers null - which
            // is the same answer a setting that is not in the schema gives. Both callers treat null
            // as "the renderer has not said", and both default to the game's own behaviour.
            document?.entrySet()?.forEach { (key, value) -> cached[key] = value }

            cachedAt = now
        }

        return cached[name]
    }

    /** The value of a bool setting, or null when the renderer has no such setting (yet). */
    @JvmStatic
    fun bool(name: String): Boolean? = runCatching {
        entry(name)?.asJsonObject?.getAsJsonPrimitive("value")?.asBoolean
    }.getOrNull()

    /** The value of an int setting, or null when the renderer has no such setting (yet). */
    @JvmStatic
    fun int(name: String): Int? = runCatching {
        entry(name)?.asJsonObject?.getAsJsonPrimitive("value")?.asInt
    }.getOrNull()

    /**
     * The index of the selected variant of an enum setting, or null when the renderer has no such
     * setting.
     *
     * An enum goes over the bridge as `{"type": "enum", "selected": n}`, where `n` counts the
     * variants in the order the schema lists them - the same order the options screen builds its
     * labels from.
     */
    @JvmStatic
    fun enumIndex(name: String): Int? = runCatching {
        entry(name)?.asJsonObject?.getAsJsonPrimitive("selected")?.asInt
    }.getOrNull()
}
