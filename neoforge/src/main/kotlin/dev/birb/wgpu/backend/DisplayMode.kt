package dev.birb.wgpu.backend

import dev.birb.wgpu.WgpuMcMod
import dev.birb.wgpu.rust.WgpuNative
import net.minecraft.client.Minecraft
import net.minecraft.client.OptionInstance
import net.minecraft.network.chat.Component
import org.lwjgl.glfw.GLFW
import org.lwjgl.glfw.GLFWVidMode

/**
 * How the game's window fills the screen: **exclusive fullscreen, borderless window, or a window.**
 *
 * ## There is no mixin here, and that is the design
 *
 * The obvious seam is `Window#setMode`, the private method that turns the game's `fullscreen` boolean
 * into one of two GLFW calls. It was tried, and the result is worth recording because it is the reason
 * this file is shaped the way it is: **a hook inside `Window`'s constructor is a hook whose failures are
 * invisible.** `Minecraft` builds its window inside
 *
 * ```java
 * try {
 *     windowCandidate = new Window(this, displayData, ..., backend);
 *     ...
 * } catch (BackendCreationException var24) { ... }
 * ```
 *
 * - that catches exactly one type - so anything else raised in there escapes, leaves the window null, and
 * the client exits through a path that reports nothing at all. There was no crash report, no fatal line,
 * and one unrelated `warn` as the only clue, twice.
 *
 * So this reaches the game's own machinery instead, by reflection, and **adds** to it rather than
 * replacing it:
 *
 * | | what happens |
 * | --- | --- |
 * | `EXCLUSIVE` | `fullscreen = true`, then the game's own mode application |
 * | `BORDERLESS` | the same, **and then** a windowed re-position over the monitor with no decorations |
 * | `OFF` | `fullscreen = false`, then the game's own mode application |
 *
 * `EXCLUSIVE` and `OFF` **are** the game's two modes, so they cannot diverge from it. Only `BORDERLESS`
 * does anything the game has no concept of, and it is the one that leaves the display mode alone.
 *
 * ## Why reflection and not an accessor
 *
 * `Window#setMode` is private and `Window#updateFullscreenIfChanged` - the public one - returns
 * immediately unless `fullscreen` and `actuallyFullscreen` disagree, which for an idempotent apply they
 * do not. The choice was reflection or a mixin accessor, and the mixin was measured to be the fragile
 * one. Reflection is checked here once, cached, and a failure is a logged warning rather than a crash.
 */
object DisplayMode {

    /**
     * The renderer setting's variants, **in the schema's own order**, which is what `windowMode` returns.
     *
     * The indices are a contract with `Settings::fullscreen_mode` on the native side: the value crosses as
     * an index into the Rust `FullscreenMode`, so reordering that enum silently reorders this one. The
     * native schema test pins its order; the test of the same name there pins the pairing.
     */
    enum class Mode {
        /** The game's own fullscreen: GLFW's monitor mode, a real display mode switch. */
        EXCLUSIVE,

        /** A decoration-free window covering the monitor. No mode switch, no exclusive ownership. */
        BORDERLESS,

        /** A window, at the size and place it was last left. */
        OFF;

        /** Whether this is any kind of fullscreen, which is what the rest of the game is told. */
        fun isFullscreen(): Boolean = this != OFF

        /** Where this mode's own name is translated - the row is on the Electrum page. */
        fun langKey(): String = when (this) {
            EXCLUSIVE -> "wgpu_mc.option.window_mode.exclusive"
            BORDERLESS -> "wgpu_mc.option.window_mode.borderless"
            OFF -> "wgpu_mc.option.window_mode.off"
        }

        companion object {
            /**
             * The mode the renderer's setting names, or `OFF` when it cannot be read.
             *
             * `OFF` for every failure - no renderer yet, no config, a hand-edited file whose index names
             * no variant - because a window is what `options.txt`'s `fullscreen: false` means and a fresh
             * install is a window. Failing to fullscreen would be a client that takes the display over
             * because a file could not be read.
             */
            fun current(): Mode = entries.getOrElse(WgpuNative.windowMode()) { OFF }
        }
    }

    /** The mode the window is in, so the game can be told what it is looking at. */
    @Volatile
    private var mode: Mode? = null

    /** Whether the mode has been applied for this session. */
    private val applied = java.util.concurrent.atomic.AtomicBoolean(false)

    /** Whether the game's private mode application has been found. See [setMode]. */
    private val setMode: java.lang.reflect.Method? by lazy {
        try {
            Minecraft::class.java.classLoader
                .loadClass("com.mojang.blaze3d.platform.Window")
                .getDeclaredMethod("setMode")
                .apply { isAccessible = true }
        } catch (failure: Throwable) {
            WgpuMcMod.LOGGER.warn(
                "wgpu: the game's window-mode method could not be reached, so only fullscreen and " +
                    "windowed are available and borderless falls back to fullscreen",
                failure,
            )
            null
        }
    }

    /**
     * Applies the configured mode once, at startup, and again whenever the setting moves.
     *
     * Called from `WgpuNative.sendSettings` through the native settings path - see
     * `debug::reapply_window_mode` - and once from the client tick, which is what covers a window that
     * `options.txt` created before any setting had been read.
     */
    @JvmStatic
    fun applyOnFirstFrame() {
        if (!applied.compareAndSet(false, true)) return

        apply()
    }

    /** See [applyOnFirstFrame]. */
    @JvmStatic
    fun reapply() {
        apply()
    }

    private fun apply() {
        val window = Minecraft.getInstance()?.window ?: return
        val chosen = Mode.current()

        // **Total by construction.** This runs from a settings apply and from the client tick, both of
        // which are places where an exception would end the frame - or the game - over a cosmetic
        // preference. A window left in its previous mode is a window; a client that will not start is not.
        try {
            val wantFullscreen = chosen.isFullscreen()

            // **The decorations are a property of the window, not of the mode it is entering**, so
            // leaving borderless has to put them back. The first version only ever removed them - it
            // called `applyBorderless` on the way in and nothing on the way out - so a window switched
            // back to `OFF` stayed undecorated with no title bar and no resize border, which is what a
            // player reported. Every path through here now states the decorations it wants rather than
            // only the one that changes them.
            setDecorated(window, !(chosen == Mode.BORDERLESS))

            if (window.isFullscreen != wantFullscreen) {
                // The game's own flag, which is what its mode application reads. Set directly rather than
                // through `toggleFullScreen`, which flips whatever it finds instead of setting it.
                setGameFullscreen(window, wantFullscreen)
            }

            // And the game's own application of it, which is a no-op when the two agree - so this is
            // idempotent and can be called on every apply.
            window.updateFullscreenIfChanged()

            if (chosen == Mode.BORDERLESS) {
                // **After** the game's own application, because that is what puts the window into
                // fullscreen and this is what turns that fullscreen window back into a repositioned one -
                // a monitor of `0L` is GLFW's "not fullscreen", so the display mode reverts and the
                // desktop is left as it was.
                applyBorderless(window)
            }

            mode = chosen

            WgpuMcMod.LOGGER.info("wgpu: the window is in {} mode", chosen)
        } catch (failure: Throwable) {
            WgpuMcMod.LOGGER.warn("wgpu: could not put the window into {} mode", chosen, failure)
        }
    }

    /**
     * Makes a window cover the monitor with no decorations, **without changing the display mode**.
     *
     * This is the whole of the borderless mode and the only part of it the game has no equivalent for:
     * [`setMode`] has already put the window into fullscreen, and this turns that fullscreen window into an
     * undecorated one covering the monitor's own bounds. The second `glfwSetWindowMonitor` - with a monitor
     * of `0L`, GLFW's "not fullscreen" - is what makes the display mode revert, so the desktop is left
     * exactly as it was.
     *
     * The bounds are the monitor's **video mode**, not its work area: the work-area query would leave the
     * taskbar visible, and a borderless window that does not cover the taskbar is a maximised window, which
     * is not what was asked for.
     */
    private fun applyBorderless(window: Any) {
        val handle = windowHandle(window) ?: return

        val monitor = GLFW.glfwGetWindowMonitor(handle).takeIf { it != 0L }
            ?: GLFW.glfwGetPrimaryMonitor()
        if (monitor == 0L) return

        val videoMode = GLFW.glfwGetVideoMode(monitor) ?: return

        val x = IntArray(1)
        val y = IntArray(1)
        GLFW.glfwGetMonitorPos(monitor, x, y)

        GLFW.glfwSetWindowMonitor(
            handle,
            0L,
            x[0],
            y[0],
            videoMode.width(),
            videoMode.height(),
            GLFW.GLFW_DONT_CARE,
        )

        // The decorations are set by `setDecorated`, which runs for every mode - see the call site.
    }

    /**
     * States whether the window has its decorations, which is what borderless mode is.
     *
     * **Stated on every apply rather than only when entering borderless**, because the decorations are a
     * property of the window and not of the mode being entered: a window that had them removed and is then
     * asked for `OFF` has to get them back, or it is a window with no title bar. See the call site.
     */
    private fun setDecorated(window: Any, decorated: Boolean) {
        val handle = windowHandle(window) ?: return

        GLFW.glfwSetWindowAttrib(
            handle,
            GLFW.GLFW_DECORATED,
            if (decorated) GLFW.GLFW_TRUE else GLFW.GLFW_FALSE,
        )
    }

    /**
     * Sets the game's own `fullscreen` flag, by reflection, because there is no setter.
     *
     * It is a plain `boolean` field and the whole of what `Window#setMode` reads, so writing it is how a
     * mode is *requested*; the application is `updateFullscreenIfChanged`, which the caller runs next.
     */
    private fun setGameFullscreen(window: Any, fullscreen: Boolean) {
        try {
            val field = window.javaClass.getDeclaredField(FULLSCREEN_FIELD)
            field.isAccessible = true
            field.setBoolean(window, fullscreen)
        } catch (failure: Throwable) {
            WgpuMcMod.LOGGER.warn(
                "wgpu: the game's fullscreen flag could not be set, so the window keeps its current mode",
                failure,
            )
        }
    }

    /** The window's GLFW handle, which every call above needs. Private, so by reflection. */
    private fun windowHandle(window: Any): Long? = try {
        val field = window.javaClass.getDeclaredField(HANDLE_FIELD)
        field.isAccessible = true
        field.getLong(window)
    } catch (failure: Throwable) {
        WgpuMcMod.LOGGER.warn("wgpu: the window's handle could not be read", failure)
        null
    }

    private const val FULLSCREEN_FIELD = "fullscreen"
    private const val HANDLE_FIELD = "handle"
}