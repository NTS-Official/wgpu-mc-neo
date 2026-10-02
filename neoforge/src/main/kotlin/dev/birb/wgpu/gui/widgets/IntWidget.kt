package dev.birb.wgpu.gui.widgets

import dev.birb.wgpu.gui.WidgetRenderer
import dev.birb.wgpu.gui.options.IntOption
import dev.birb.wgpu.gui.options.Option
import net.minecraft.locale.Language
import net.minecraft.network.chat.Component
import net.minecraft.network.chat.FormattedText

class IntWidget(x: Int, y: Int, width: Int, private val option: IntOption) :
    Widget(x, y, width, Widget.DEFAULT_HEIGHT), IOptionWidget {

    private var dragging = false

    override fun getOption(): Option<*> = option

    /**
     * Whether this row can be used right now, read afresh each time.
     *
     * Delegated to the option because the answer belongs to the setting rather than to the widget: see
     * `IntOption.enabledWhen`, which is where the case it exists for is written down (`maxAnisotropy` is
     * only meaningful while `textureFiltering` is `ANISOTROPIC`).
     */
    private fun enabled(): Boolean = option.isEnabled()

    override fun mouseClicked(mouseX: Double, mouseY: Double, button: Int): Boolean {
        // A disabled row refuses the click rather than taking it and doing nothing, which is what vanilla
        // does and what keeps the drag state from being entered on a slider that will not move.
        if (!enabled()) return false

        if (isMouseOver(mouseX, mouseY)) {
            dragging = true
            calculateValue(mouseX.toInt())
            return true
        }
        return false
    }

    override fun mouseReleased(mouseX: Double, mouseY: Double, button: Int): Boolean {
        if (dragging) {
            dragging = false
            return true
        }
        return false
    }

    override fun mouseMoved(mouseX: Double, mouseY: Double) {
        if (dragging) calculateValue(mouseX.toInt())
    }

    override fun mouseDragged(mouseX: Double, mouseY: Double, button: Int, dragX: Double, dragY: Double): Boolean {
        if (dragging) {
            calculateValue(mouseX.toInt())
            return true
        }
        return false
    }

    private fun calculateValue(mouseX: Int) {
        val fraction = fractionAt(mouseX)

        // A setting that knows its own slider is asked through it, because only it knows the values
        // it accepts: asking for one it rejects is not a value that gets clamped, it is a value that
        // gets logged and thrown away - see `IntSlider`.
        val slider = option.slider
        if (slider != null) {
            option.set(slider.at(fraction))
            return
        }

        val value = fraction * (option.max - option.min) + option.min
        option.set((Math.round(value / option.step) * option.step).toInt())
    }

    /** Where in the track a mouse position is, as 0..1. The track is the value half of the row. */
    private fun fractionAt(mouseX: Int): Double {
        val track = (width / 2 - 6).coerceAtLeast(1)
        return ((mouseX - x - width / 2).toDouble() / track).coerceIn(0.0, 1.0)
    }

    override fun render(renderer: WidgetRenderer, mouseX: Int, mouseY: Int, delta: Float) {
        // **A row another row decides does not hover.** The hover state is what draws the track and the
        // handle, and drawing a slider a player cannot drag is the control inviting a click it will
        // refuse - so the row keeps its background, drops the hover, and greys its text. See
        // `Widget.DISABLED` for what it does *not* do: leave the row out. A setting that vanished when
        // another row moved would be a setting a player cannot read the value of.
        val usable = enabled()
        val hovered = usable && (isMouseOver(mouseX, mouseY) || dragging)

        // Background
        renderer.rect(x, y, x + width, y + height, if (hovered) Widget.BG_HOVERED else Widget.BG)

        val textColor = if (usable) Widget.WHITE else Widget.DISABLED

        val halfWidth = width / 2

        // Name
        if (hovered && renderer.textWidth(option.displayName()) > width / 3) {
            val trimmed = FormattedText.composite(renderer.trimText(option.displayName(), width / 3), Component.literal("..."))
            renderer.text(Language.getInstance().getVisualOrder(trimmed), x + 6, centerTextY(renderer), textColor)
        } else {
            renderer.text(option.displayName(), x + 6, centerTextY(renderer), textColor)
        }

        // Value, through the option's own formatter - which for a disabled row is what says what the
        // setting *is* rather than what it would do: `maxAnisotropy`'s own formatter reads `4x` whether or
        // not the answer above it makes that value reach the sampler.
        val valueText = if (hovered) Component.literal(option.get().toString()) else option.formatter.apply(option.get())
        renderer.text(valueText, alignRight(renderer.textWidth(valueText), if (hovered) halfWidth else width), centerTextY(renderer), textColor)

        if (hovered) {
            // Track
            renderer.rect(x + halfWidth, centerY(1), x + width - 6, centerY(1) + 1, Widget.WHITE)

            // Handle
            val handleX = x + halfWidth + getHandleX()
            val h = renderer.textHeight() + 2
            renderer.rect(handleX, centerY(h), handleX + 3, centerY(h) + h, Widget.WHITE)
        }
    }

    private fun getHandleX(): Int {
        val slider = option.slider
        val delta = if (slider != null) {
            slider.fraction(option.get())
        } else {
            (option.get() - option.min).toDouble() / (option.max - option.min)
        }

        return (delta * (width / 2 - 6)).toInt() - 1
    }
}
