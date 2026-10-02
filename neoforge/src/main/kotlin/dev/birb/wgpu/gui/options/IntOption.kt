package dev.birb.wgpu.gui.options

import dev.birb.wgpu.gui.widgets.IntWidget
import dev.birb.wgpu.gui.widgets.Widget
import net.minecraft.network.chat.Component
import java.util.function.Consumer
import java.util.function.Function
import java.util.function.Supplier

class IntOption(
    name: Component,
    tooltip: Component,
    requiresRestart: Boolean,
    getter: Supplier<Int>,
    setter: Consumer<Int>,
    val min: Int,
    val max: Int,
    val step: Int = 1,
    formatter: Function<Int, Component> = STANDARD_FORMATTER,
    /**
     * The values the setting accepts, when something else owns them - a vanilla option does, and its
     * range and step are not the ones this side would guess. `null` leaves the slider on [min],
     * [max] and [step], which is all a setting this mod owns needs.
     */
    val slider: IntSlider? = null,
    /**
     * Whether this row can be used, read afresh every frame - or `null` for a row nothing else decides.
     *
     * A **predicate rather than a value** because the answer can change while the screen is open and it is
     * not this row that changes it: `options.maxAnisotropy` is only meaningful while
     * `options.textureFiltering` says `ANISOTROPIC`, and the row above it is the one that moves. Reading
     * the other option's state on each draw is what makes the two rows agree in the same frame, before
     * Apply - a value captured when the page was built would grey the row on what the setting was when the
     * screen opened.
     *
     * It is read from the **other option** and not from this row's `value`, which matters: a row the player
     * has edited but not applied is showing a value the game does not have yet, and greying a row on the
     * strength of a pending edit would be the screen disagreeing with itself.
     */
    val enabledWhen: (() -> Boolean)? = null
) : Option<Int>(name, tooltip, requiresRestart, getter, setter) {

    val formatter: Function<Int, Component> = formatter

    /** See [enabledWhen]. */
    fun isEnabled(): Boolean = enabledWhen?.invoke() ?: true

    override fun createWidget(x: Int, y: Int, width: Int): Widget {
        return IntWidget(x, y, width, this)
    }

    class Builder : Option.Builder<Builder, Int>() {
        private var formatter: Function<Int, Component> = STANDARD_FORMATTER
        private var min: Int = 0
        private var max: Int = 0
        private var step: Int = 1
        private var enabledWhen: (() -> Boolean)? = null

        fun setFormatter(formatter: Function<Int, Component>): Builder {
            this.formatter = formatter
            return this
        }

        fun setRange(min: Int, max: Int): Builder {
            this.min = min
            this.max = max
            return this
        }

        fun setStep(step: Int): Builder {
            this.step = step
            return this
        }

        /** Draws this row as unusable whenever [predicate] answers false. See [IntOption.enabledWhen]. */
        fun setEnabledWhen(predicate: () -> Boolean): Builder {
            this.enabledWhen = predicate
            return this
        }

        override fun build(): Option<Int> {
            return IntOption(
                requireName(),
                resolveTooltip(),
                requiresRestart,
                requireGetter(),
                requireSetter(),
                min,
                max,
                step,
                formatter,
                slider,
                enabledWhen
            )
        }
    }

    companion object {
        val STANDARD_FORMATTER = Function<Int, Component> { integer -> Component.literal(integer.toString()) }
    }
}
