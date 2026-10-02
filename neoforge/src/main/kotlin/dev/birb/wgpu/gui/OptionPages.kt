package dev.birb.wgpu.gui

import com.google.gson.GsonBuilder
import com.google.gson.reflect.TypeToken
import dev.birb.wgpu.WgpuMcMod
import dev.birb.wgpu.backend.Diagnostics
import dev.birb.wgpu.backend.DisplayMode
import dev.birb.wgpu.gui.options.*
import dev.birb.wgpu.gui.widgets.HeadingWidget
import dev.birb.wgpu.gui.widgets.Widget
import dev.birb.wgpu.rust.RendererSettings
import dev.birb.wgpu.rust.WgpuNative
import net.minecraft.client.AttackIndicatorStatus
import net.minecraft.client.CloudStatus
import net.minecraft.client.GraphicsPreset
import net.minecraft.client.InactivityFpsLimit
import net.minecraft.client.Minecraft
import net.minecraft.client.PrioritizeChunkUpdates
import net.minecraft.client.TextureFilteringMethod
import net.minecraft.client.resources.language.I18n
import net.minecraft.network.chat.Component
import net.minecraft.network.chat.contents.TranslatableContents
import net.minecraft.server.level.ParticleStatus

class OptionPages : Iterable<OptionPages.Page> {
    private val pages: MutableList<Page> = ArrayList()

    init {
        pages.add(createGeneral())
        pages.add(createElectrum())
        pages.add(createQuality())
        reportMissingTranslations()
    }

    /**
     * Names every key the screen asks for that the loaded language does not have.
     *
     * A key that resolves to nothing is drawn as itself, so a row whose translation went missing is
     * labelled `options.graphics` - and there is no compile error to catch it, because the key is a
     * string on both sides of the lookup. This is not hypothetical: 26.1 renamed that very option to
     * `options.graphics.preset` and put the old key in `assets/minecraft/lang/deprecated.json`'s
     * `removed` list, which `DeprecatedTranslationsInfo` *strips* from every language file, so the
     * old key resolves to nothing in every language, including the ones that still spell it out.
     *
     * A key that carries a fallback is skipped, because falling back is what this mod's own
     * descriptions do on purpose - see `OptionText`. What is left is exactly the keys that are
     * supposed to resolve, which is what makes this worth a warning rather than a debug line.
     */
    private fun reportMissingTranslations() {
        if (!TRANSLATIONS_REPORTED.compareAndSet(false, true)) {
            return
        }

        val missing = LinkedHashSet<String>()

        fun check(component: Component) {
            val contents = component.contents
            if (contents !is TranslatableContents) return
            if (contents.fallback != null) return
            if (!I18n.exists(contents.key)) missing.add(contents.key)
        }

        for (page in pages) {
            check(page.name)

            for (group in page) {
                for (entry in group) {
                    when (entry) {
                        is Entry.Heading -> check(entry.text)
                        is Entry.Setting -> {
                            check(entry.option.name)
                            check(entry.option.tooltip)
                        }
                    }
                }
            }
        }

        if (missing.isNotEmpty()) {
            WgpuMcMod.LOGGER.warn(
                "wgpu: {} key(s) the options screen asks for are not in the loaded language, so " +
                    "those rows show the key itself: {}",
                missing.size,
                missing.joinToString(", "),
            )
        }
    }

    fun getDefault(): Page = pages[0]

    private var appliedRestartChanges = false

    fun isChanged(): Boolean = pages.any { it.isChanged() }

    /**
     * Whether anything that has been edited only takes effect on the next launch.
     *
     * The renderer's own settings mostly work this way: the graphics backend, for instance, picks
     * the wgpu instance, the adapter and every resource underneath them, and none of that can be
     * swapped out while the game is running.
     */
    fun hasPendingRestartChanges(): Boolean = pages.any { it.hasPendingRestartChanges() }

    /**
     * Whether anything that has been applied - as opposed to merely edited - only takes effect on
     * the next launch. Read after [apply], so the notice survives the edit being committed.
     */
    fun hasAppliedRestartChanges(): Boolean = appliedRestartChanges

    fun apply() {
        if (hasPendingRestartChanges()) appliedRestartChanges = true

        // The renderer's settings go back as **one document**, and it holds every row on every page
        // that names one - not the rows of the page being applied.
        //
        // That is the renderer's design rather than a convenience here: the document *is* its config,
        // every field of it has a serde default, and `sendSettings` parses what it is given as the
        // whole thing - so a page that sent only its own rows would silently reset every setting on
        // the other pages. It used to be safe by accident, because the renderer's rows were all on
        // one page; it stopped being safe the day the animated-texture switch moved to the Quality
        // page, which is mostly Minecraft's own options. See `Page.apply`, which no longer sends.
        val rendererOptions = pages.flatMap { it.rendererOptions() }

        if (rendererOptions.isNotEmpty()) {
            val json = GSON.toJson(rendererOptions, SETTINGS_TYPE_TOKEN.type)
            if (!WgpuNative.sendSettings(json)) {
                // Nothing is applied on a failed save: half of an Apply is worse than none of it, and
                // the values the screen is showing are still the ones the player asked for.
                WgpuMcMod.LOGGER.error("Failed to save the renderer settings")
                return
            }

            // `sendSettings` applies what it can immediately - `vsync` reconfigures the swapchain, the
            // debug switches are read on the next draw, and a setting that is *baked* into geometry
            // asks this side for a re-bake - so by the time this returns, the renderer is already
            // running with the new values. This side has its own copy of the diagnostics switch,
            // because it is the side that dumps frames.
            Diagnostics.refresh()
            syncVanillaVsync(rendererOptions)
        }

        pages.forEach { it.apply() }
    }

    fun undo() = pages.forEach { it.undo() }

    override fun iterator(): Iterator<Page> = pages.iterator()

    private fun createGeneral(): Page {
        val page = Page(Component.translatable("wgpu_mc.page.general"))
        val mc = Minecraft.getInstance()
        val options = mc.options

        // The slider behaviour of a vanilla row comes from the option itself (`Option.Builder
        // .setOption` picks it up), so the range written here is only what would be used if that
        // ever came back empty - but it should still be the option's own range rather than one that
        // looks plausible: `simulationDistance` really does go to 32 on a machine with the memory
        // for it, and `framerateLimit` really is 10..260 in tens.
        page.add(IntOption.Builder()
            .setName(Component.translatable("options.renderDistance"))
            .setOption(options.renderDistance())
            .setFormatter { integer -> Component.translatable("options.chunks", integer) }
            .setRange(2, 32)
            .build())

        page.add(IntOption.Builder()
            .setName(Component.translatable("options.simulationDistance"))
            .setOption(options.simulationDistance())
            .setFormatter { integer -> Component.translatable("options.chunks", integer) }
            .setRange(5, 32)
            .build())

        page.add(IntOption.Builder()
            .setName(Component.translatable("options.gamma"))
            .setAccessors(
                { (options.gamma().get() * 100).toInt() },
                { integer -> options.gamma().set(integer / 100.0) }
            )
            .setFormatter { integer ->
                when (integer) {
                    0 -> Component.translatable("options.gamma.min")
                    50 -> Component.translatable("options.gamma.default")
                    100 -> Component.translatable("options.gamma.max")
                    else -> Component.literal("$integer%")
                }
            }
            .setRange(0, 100)
            .build())

        page.space()
        page.add(IntOption.Builder()
            .setName(Component.translatable("options.guiScale"))
            .setOption(options.guiScale()) { mc.resizeGui() }
            .setFormatter { integer ->
                // Vanilla's own word for it, which every language already has.
                if (integer == 0) Component.translatable("options.guiScale.auto")
                else Component.literal("${integer}x")
            }
            .setRange(0, 4)
            .build())

        // **The window mode, in place of the game's fullscreen checkbox**, and this page is where it has
        // to be: `OptionsScreenMixin` replaces the whole video settings screen with this one, so a row
        // injected into `VideoSettingsScreen` is a row on a screen nothing opens. That was measured, at
        // the cost of a round of "the option is still a checkbox".
        //
        // Three states rather than the game's two, because `Options#fullscreen` is a boolean whose handler
        // calls `Window#toggleFullScreen` - it can only mean the game's own fullscreen or windowed, and
        // there is no third state for a window that covers the monitor without owning the display mode.
        //
        // `options.fullscreen()` itself is left alone and still written by the game's F11 handler, so the
        // value in `options.txt` keeps meaning what it always did. The mode is this renderer's setting,
        // stored in its own config; the row is the one the game's checkbox used to occupy, under the same
        // caption, so a player finds it where they always did.
        page.add(EnumOption.Builder(DisplayMode.Mode::class.java)
            .setName(Component.translatable("options.fullscreen"))
            .setTooltip(Component.translatable("wgpu_mc.option.window_mode.tooltip"), false)
            .setAccessors(
                { DisplayMode.Mode.current() },
                { mode -> WgpuNative.setFullscreenMode(mode.ordinal) }
            )
            .setFormatter { mode -> Component.translatable(mode.langKey()) }
            .build())

        // Vanilla's VSync toggle is deliberately not offered here. Its only effect on this backend
        // would be through `GpuDevice#setVsync`, which this renderer ignores on purpose - the
        // present mode belongs to the Electrum tab's `vsync` setting, which applies without a
        // restart. The vanilla option itself is kept in step with that setting, because other mods
        // and the F3 overlay read it, but leaving a switch in the list that changes nothing would
        // be worse than leaving it out.

        page.add(IntOption.Builder()
            .setName(Component.translatable("options.framerateLimit"))
            .setOption(options.framerateLimit())
            .setFormatter { integer ->
                if (integer == 260) Component.translatable("options.framerateLimit.max")
                else Component.literal(integer.toString())
            }
            // Vanilla stores this one as 1..26 and shows it as 10..260, so its slider only ever
            // produces multiples of ten - asking for 5 was a value the option rejected outright.
            .setRange(10, 260)
            .setStep(10)
            .build())

        // The other half of the frame-rate pair in vanilla's Display group, and the renderer already
        // has the field for it: `Options#inactivityFpsLimit` is what throttles the game when the
        // window is minimized or the player is away, and `settings.rs` carries `inactivity_fps_limit`
        // beside `max_fps` for exactly that. Until this row existed the stored value was still
        // honoured by the game, but a player had no way to change it from inside the renderer's own
        // video settings page - which is the only video settings page this backend opens.
        page.add(EnumOption.Builder(InactivityFpsLimit::class.java)
            .setName(Component.translatable("options.inactivityFpsLimit"))
            .setOption(options.inactivityFpsLimit())
            .setFormatter { limit -> limit.caption() }
            .build())

        page.space()
        page.add(BoolOption.Builder()
            .setName(Component.translatable("options.viewBobbing"))
            .setOption(options.bobView())
            .build())

        page.add(EnumOption.Builder(AttackIndicatorStatus::class.java)
            .setName(Component.translatable("options.attackIndicator"))
            .setOption(options.attackIndicator())
            .setFormatter { status -> status.caption() }
            .build())

        page.add(BoolOption.Builder()
            .setName(Component.translatable("options.autosaveIndicator"))
            .setOption(options.showAutosaveIndicator())
            .build())

        // **How much the menu background is blurred, which reaches this screen as well as the game's own.**
        // It is pure interface code - no picture the renderer draws depends on it - and this screen already
        // gets the game's background through `Screen#extractBackground`, which is what applies the blur, so
        // the row is the whole of it. Vanilla lists the same option on the Accessibility page too, which
        // this backend does not replace, so a player can reach it from either place.
        page.add(IntOption.Builder()
            .setName(Component.translatable("options.accessibility.menu_background_blurriness"))
            .setOption(options.menuBackgroundBlurriness())
            .setFormatter { amount ->
                if (amount == 0) Component.translatable("options.off")
                else Component.literal(amount.toString())
            }
            .build())

        return page
    }

    private fun createElectrum(): Page {
        val page = Page(Component.translatable("wgpu_mc.page.electrum"))
        val rustSettings = WgpuNative.getSettings()
        val options: List<Option<*>> = GSON.fromJson(rustSettings, SETTINGS_TYPE_TOKEN.type)

        // Which settings belong to a section is the renderer's answer, not this side's: the schema
        // marks the debug switches with a section name, and the page draws the heading when it
        // reaches the first one of them. A blank row goes above the heading, so it reads as a break
        // in the list rather than as a label on the setting before it.
        var section: String? = null
        for (option in options) {
            // A setting this screen draws on another page is skipped here: one row, one place. The
            // animated-texture switch is on the Quality page, next to the graphics preset it is a
            // sibling of, and it must not also appear at the bottom of this list.
            if (option.setting in DRAWN_ELSEWHERE) {
                continue
            }

            val optionSection = SETTINGS_STRUCTURE[option.setting]?.section

            if (optionSection != null && optionSection != section) {
                section = optionSection
                page.blankRow()
                page.header(Component.translatableWithFallback(OptionText.sectionKey(optionSection), optionSection))
            }

            page.add(option)
        }

        return page
    }

    private fun createQuality(): Page {
        val page = Page(Component.translatable("wgpu_mc.page.quality"))
        val options = Minecraft.getInstance().options

        page.add(EnumOption.Builder(GraphicsPreset::class.java)
            // 26.1 renamed this option: `options.graphics` is in the deprecated list, and Minecraft
            // *strips* deprecated keys from every language file, so the old key resolves to nothing
            // and the row was labelled with the key itself. `options.graphics.preset` is the live one.
            .setName(Component.translatable("options.graphics.preset"))
            .setOption(options.graphicsPreset())
            .setFormatter { graphicsPreset -> Component.translatable(graphicsPreset.getKey()) }
            // Fabulous is missing on purpose: it is the preset that turns on improved transparency,
            // whose post chain this backend cannot bind yet - see GraphicsPresets, which also clamps a
            // settings file that still names it.
            .setValues(GraphicsPresets.offered())
            .build())

        page.space()
        page.add(EnumOption.Builder(CloudStatus::class.java)
            .setName(Component.translatable("options.renderClouds"))
            .setOption(options.cloudStatus())
            .setFormatter { cloudStatus -> cloudStatus.caption() }
            .build())

        // How far the clouds are drawn, in chunks - the other half of the cloud row above, and the
        // option vanilla's own Quality group puts beside it. Its range and step come from the option
        // itself (`IntSlider.of` in `Option.Builder#setOption`), so nothing here guesses at them.
        page.add(IntOption.Builder()
            .setName(Component.translatable("options.renderCloudsDistance"))
            .setOption(options.cloudRange())
            .setFormatter { integer -> Component.translatable("options.chunks", integer) }
            .build())

        // How far the rain and snow are drawn, in blocks. Only the weather *rendering* is this
        // renderer's business here: the row sets the game's own option and the game's own weather
        // pass reads it, which is how every other non-terrain option on this page works.
        page.add(IntOption.Builder()
            .setName(Component.translatable("options.weatherRadius"))
            .setOption(options.weatherRadius())
            .setFormatter { integer -> Component.translatable("options.blocks", integer) }
            .build())

        // Whether leaves are drawn with their cut-out texels tested or as solid cubes. The game
        // re-meshes every section when this flips (`LevelRenderer::allChanged`), and this renderer's
        // bake reads the same option - `FACES_FORCED_OPAQUE` in `mc/chunk.rs` is the branch it takes,
        // and a leaves face that goes through it is counted rather than silently drawn.
        page.add(BoolOption.Builder()
            .setName(Component.translatable("options.cutoutLeaves"))
            .setOption(options.cutoutLeaves())
            .build())

        page.add(EnumOption.Builder(ParticleStatus::class.java)
            .setName(Component.translatable("options.particles"))
            .setOption(options.particles())
            .setFormatter { particleStatus -> particleStatus.caption() }
            .build())

        // The renderer's own row on this page: whether the block textures Minecraft animates - fire,
        // lava, the campfire - move on the terrain this renderer draws. A quality choice, and it sits
        // with the ones the graphics preset moves.
        //
        // It is the only row here that is not Minecraft's, which is what makes this page a *mixed*
        // one: see `OptionPages.apply` for why that matters to how the settings are sent, and
        // `DRAWN_ELSEWHERE` for why the Neolectrum page skips it.
        val animatedTextures = rendererSetting(ANIMATED_TEXTURES)

        if (animatedTextures == null) {
            // Not fatal, and not silent: a row that is missing because the renderer's schema did not
            // arrive is otherwise a setting that looks like it does not exist.
            WgpuMcMod.LOGGER.warn(
                "wgpu: the renderer has no `{}` setting to draw on the Quality page",
                ANIMATED_TEXTURES,
            )
        } else {
            page.add(animatedTextures)
        }

        page.add(BoolOption.Builder()
            .setName(Component.translatable("options.ao"))
            .setOption(options.ambientOcclusion())
            .build())

        page.add(IntOption.Builder()
            .setName(Component.translatable("options.biomeBlendRadius"))
            .setOption(options.biomeBlendRadius())
            .setFormatter { integer -> Component.translatable("options.biomeBlendRadius.${integer * 2 + 1}") }
            .setRange(0, 7)
            .build())

        page.space()
        page.add(IntOption.Builder()
            .setName(Component.translatable("options.entityDistanceScaling"))
            .setAccessors(
                { (options.entityDistanceScaling().get() * 100).toInt() },
                { integer -> options.entityDistanceScaling().set(integer / 100.0) }
            )
            .setFormatter { integer -> Component.literal("$integer%") }
            .setRange(50, 500)
            .setStep(25)
            .build())

        page.add(BoolOption.Builder()
            .setName(Component.translatable("options.entityShadows"))
            .setOption(options.entityShadows())
            .build())

        page.space()
        page.add(IntOption.Builder()
            .setName(Component.translatable("options.mipmapLevels"))
            .setOption(options.mipmapLevels())
            .setFormatter { integer -> Component.literal("${integer}x") }
            .setRange(0, 4)
            .build())

        // **Where a chunk rebuild goes when the player is waiting on one.** The row is Minecraft's and
        // Minecraft's own section dispatcher reads it - which is the dispatcher the terrain arena is
        // fed from (`BlockCache`), so the choice reaches this renderer along the path it already had:
        // a rebuild the option defers is a rebuild this side is asked for later.
        page.add(EnumOption.Builder(PrioritizeChunkUpdates::class.java)
            .setName(Component.translatable("options.prioritizeChunkUpdates"))
            .setOption(options.prioritizeChunkUpdates())
            .setFormatter { updates -> updates.caption() }
            .build())

        // **How the block atlas is sampled, and how much anisotropy that is worth** - the pair vanilla's
        // Quality group puts together, and they belong together here too because the renderer reads them
        // as one line of `LevelRenderer`:
        //
        // ```java
        // int maxAnisotropy = textureFiltering == ANISOTROPIC ? maxAnisotropyValue : 1;
        // ```
        //
        // `textureFiltering` was wired in the renderer before either row existed - `TerrainPass` pushes it
        // every frame and the terrain shader implements all three answers, `NONE` as a plain fetch, `RGSS`
        // as the game's four-tap rotated grid and `ANISOTROPIC` as the sampler's own filter. The bit is the
        // half that was missing: the clamp used to be the constant 4, which is the option's *default* and
        // not its value, so `options.maxAnisotropy` moved nothing. See `setMaxAnisotropyBit`.
        page.add(EnumOption.Builder(TextureFilteringMethod::class.java)
            .setName(Component.translatable("options.textureFiltering"))
            .setOption(options.textureFiltering())
            .setFormatter { method -> method.caption() }
            .build())

        // **Disabled unless the answer above is `ANISOTROPIC`, which is what vanilla does and what the
        // arithmetic says.** The sampler's anisotropy is `1` under the other two answers whatever this bit
        // is, so an enabled slider would be a control that changes nothing - and the row that *does* decide
        // it is directly above. See `setEnabledWhen`, whose subject is the *option* rather than the row
        // above it: a row the player has edited but not applied is showing a value the game does not have
        // yet, so greying this one on a pending edit would be the screen disagreeing with itself. It
        // follows the change through Apply, which is when the sampler changes anyway.
        page.add(IntOption.Builder()
            .setName(Component.translatable("options.maxAnisotropy"))
            .setOption(options.maxAnisotropyBit())
            // The option's own formatter: "Off" at 0 and `2x`, `4x`, `8x` above it, because the stored value
            // is the exponent and the player is shown the multiplier. `options.off` and `options.multiplier`
            // are Minecraft's own two strings - the game's own formatter for this option is
            // `CommonComponents.optionStatus(caption, false)` for the zero branch, and the caption half of
            // that is the row's own name, which this widget draws separately. So the zero branch here is the
            // value half of the same line, and no new translation is invented for either.
            .setFormatter { bit ->
                if (bit == 0) Component.translatable("options.off")
                else Component.translatable("options.multiplier", (1 shl bit).toString())
            }
            .setEnabledWhen { options.textureFiltering().get() == TextureFilteringMethod.ANISOTROPIC }
            .build())

        // The other two rows vanilla's Quality group has that this page does not draw, named so that the
        // gap is a decision rather than an oversight:
        //
        //  - `options.improvedTransparency` is the Fabulous preset, which `GraphicsPresets` hides with a
        //    reason of its own (the transparency post chain samples depth through a filterable float);
        //  - `options.vignette` is the same kind of post-processing the backend cannot run yet.
        //
        // `fullscreen.resolution` and `options.exclusiveFullscreen` are the display pair, covered by the
        // window-mode row on the General page and the display-mode picker `DisplayMode` replaces.

        return page
    }

    /**
     * One page of the options screen, as a list of rows in groups.
     *
     * A group is what a `space()` starts: the rows in it are drawn together, and the screen leaves a
     * small gap between groups.
     */
    class Page(val name: Component) : Iterable<List<OptionPages.Entry>> {
        private val groups: MutableList<MutableList<Entry>> = ArrayList()

        init {
            space()
        }

        fun add(option: Option<*>) = add(Entry.Setting(option))

        fun add(entry: Entry) {
            groups[groups.size - 1].add(entry)
        }

        /** A sub-heading over the settings that follow it. */
        fun header(text: Component) = add(Entry.Heading(text))

        /** A row of nothing, which is what separates a section from the setting above it. */
        fun blankRow() = add(Entry.Heading(Component.empty()))

        fun space() {
            groups.add(ArrayList())
        }

        fun isChanged(): Boolean = options().any { it.isChanged() }

        fun hasPendingRestartChanges(): Boolean =
            options().any { it.isChanged() && it.requiresRestart }

        /** Every row on this page that carries one of the renderer's settings. */
        fun rendererOptions(): List<Option<*>> = options().filter { it.setting != null }

        /**
         * Commits this page's edits.
         *
         * The renderer's settings are *sent* by [OptionPages.apply], which is the only place that sees
         * every page and can build one complete document; this applies the rows, whichever side of the
         * bridge they belong to.
         */
        fun apply() {
            // What changed is read before it is applied and reported after, because the two can
            // disagree: a vanilla option that refuses a value logs an error of its own and keeps the
            // one it had, and the row used to go on showing the value that was asked for. The line
            // below names what was applied and what each setting is *after* applying it.
            val changed = options().filter { it.isChanged() }

            options().forEach { it.apply() }

            // Then every row is read back, because applying one can change others: a graphics preset
            // sets a dozen options at once, and a page that went on showing the values from before
            // would be lying about the game it is editing.
            options().forEach { it.resync() }

            if (changed.isNotEmpty()) {
                WgpuMcMod.LOGGER.info(
                    "wgpu: applied {} video option(s): {}",
                    changed.size,
                    changed.joinToString(", ") { "${it.name.string}=${it.get()}" },
                )
            }
        }

        fun undo() = options().forEach { it.undo() }

        override fun iterator(): Iterator<List<Entry>> = groups.iterator()

        /** Every setting on the page, in the order the rows appear. Headings contribute none. */
        private fun options(): List<Option<*>> = groups.flatten().flatMap { it.options }
    }

    /**
     * One row of a page: a setting, or a heading over the settings below it.
     *
     * The screen draws rows rather than options so that a section can be part of the list while
     * still being nothing the player can set - see [Heading].
     */
    sealed interface Entry {
        fun createWidget(x: Int, y: Int, width: Int): Widget

        /** The settings this row contributes, which is none for a heading. */
        val options: List<Option<*>>

        class Setting(val option: Option<*>) : Entry {
            override fun createWidget(x: Int, y: Int, width: Int): Widget =
                option.createWidget(x, y, width)

            override val options: List<Option<*>> get() = listOf(option)
        }

        class Heading(val text: Component) : Entry {
            override fun createWidget(x: Int, y: Int, width: Int): Widget =
                HeadingWidget(x, y, width, text)

            override val options: List<Option<*>> get() = emptyList()
        }
    }

    companion object {
        private val SETTINGS_STRUCTURE_TYPE_TOKEN = object : TypeToken<Map<String, RustOptionInfo>>() {}
        private val SETTINGS_TYPE_TOKEN = object : TypeToken<List<Option<*>>>() {}

        /** The missing-translation report is worth one line per session, not one per screen. */
        private val TRANSLATIONS_REPORTED = java.util.concurrent.atomic.AtomicBoolean()

        /** The name the renderer's animated-texture setting has in the schema. */
        private const val ANIMATED_TEXTURES = "animated_textures"

        /**
         * The renderer settings this screen draws somewhere other than the Neolectrum page.
         *
         * One row belongs in one place, and the renderer's schema has no way to say *where* a setting
         * is drawn beyond the section heading it sits under on that page - so a setting that belongs
         * next to the graphics preset is named here and skipped there. The list is short by design: it
         * is the exception, and a second entry is a second thing to keep in step.
         */
        private val DRAWN_ELSEWHERE = setOf(ANIMATED_TEXTURES, WINDOW_MODE_SETTING)

        /**
         * One of the renderer's settings, as a row, for a page that is not the renderer's own.
         *
         * Read through the same deserializer the Neolectrum page uses, from the same document: the
         * name, the tooltip and the values are the ones the renderer's schema gives them, and there is
         * only one way to read them. `null` when the document has no such setting - see the caller,
         * which says so in the log rather than drawing nothing.
         */
        private fun rendererSetting(name: String): Option<*>? {
            val settings: List<Option<*>> =
                GSON.fromJson(WgpuNative.getSettings(), SETTINGS_TYPE_TOKEN.type)

            return settings.firstOrNull { it.setting == name }
        }

        private val GSON = GsonBuilder()
            .registerTypeAdapter(SETTINGS_TYPE_TOKEN.type, Option.OptionSerializerDeserializer())
            .create()

        val SETTINGS_STRUCTURE: Map<String, RustOptionInfo> = GSON.fromJson(
            WgpuNative.getSettingsStructure(),
            SETTINGS_STRUCTURE_TYPE_TOKEN.type
        )

        /** The name the renderer's own vsync setting has in the schema. */
        private const val VSYNC_SETTING = "vsync"

        /**
         * The window-mode setting's name in the renderer's schema.
         *
         * Skipped on this mod's own page because **it is drawn on Minecraft's**: the cycle button in the
         * video settings screen, added by `VideoSettingsScreenMixin` and built by
         * [dev.birb.wgpu.backend.DisplayMode.windowModeOption]. The value is still this renderer's - it
         * lives in `config/wgpu-mc-renderer.json`, not in `options.txt` - so the schema entry stays, and it
         * is what the row's name, tooltip and values come from.
         */
        const val WINDOW_MODE_SETTING = "fullscreen_mode"

        /**
         * Copies the renderer's `vsync` setting into Minecraft's own option of the same name.
         *
         * The vanilla option no longer decides anything here - the renderer's setting does, and it
         * applies without a restart - but it is not private to this mod: the F3 overlay prints
         * "vsync" from `options.enableVsync()`, and other mods read it to know whether frames are
         * being synced. Leaving it at a stale value would make both lie, so it follows ours.
         *
         * Called on apply and once at client setup, so the two agree before the first frame.
         */
        fun syncVanillaVsync(options: List<Option<*>>? = null) {
            val enabled = options?.firstOrNull { it.setting == VSYNC_SETTING }?.get() as? Boolean
                ?: rendererVsyncSetting()

            if (Minecraft.getInstance().options.enableVsync().get() != enabled) {
                Minecraft.getInstance().options.enableVsync().set(enabled)
                WgpuMcMod.LOGGER.info(
                    "wgpu: vsync is {}; Minecraft's own option of the same name follows it",
                    enabled,
                )
            }
        }

        /** The `vsync` value the renderer is running with, read back from its settings. */
        private fun rendererVsyncSetting(): Boolean =
            RendererSettings.bool(VSYNC_SETTING) ?: true
    }
}

