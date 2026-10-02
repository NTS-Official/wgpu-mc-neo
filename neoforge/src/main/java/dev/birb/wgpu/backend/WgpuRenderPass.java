package dev.birb.wgpu.backend;

import com.mojang.blaze3d.buffers.GpuBuffer;
import com.mojang.blaze3d.buffers.GpuBufferSlice;
import com.mojang.blaze3d.pipeline.RenderPipeline;
import com.mojang.blaze3d.systems.RenderPass;
import com.mojang.blaze3d.systems.RenderPassBackend;
import com.mojang.blaze3d.textures.GpuSampler;
import com.mojang.blaze3d.textures.GpuTextureView;
import com.mojang.blaze3d.vertex.VertexFormat;
import dev.birb.wgpu.render.TerrainPass;
import dev.birb.wgpu.rust.NativeNames;
import dev.birb.wgpu.rust.RendererSettings;
import dev.birb.wgpu.rust.WmNative;
import net.minecraft.util.ARGB;
import org.jspecify.annotations.NonNull;

import java.lang.foreign.Arena;
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.util.ArrayDeque;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collection;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.OptionalDouble;
import java.util.OptionalInt;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.function.Supplier;

/**
 * Blaze3D's render pass, forwarding to a {@code wgpu::RenderPass}.
 *
 * <p>This one class is deliberately written in Java rather than Kotlin. Kotlin cannot override an
 * interface method declared with an unbounded type parameter, and {@code RenderPassBackend}
 * declares {@code <T> void drawMultipleIndexed(...)}; implementing the interface in Java is the
 * only way to satisfy it. The rest of the backend stays in Kotlin.
 *
 * <p>The interesting part of the port is the descriptor marshalling: {@code
 * BlazeRenderPassDescriptor} is a struct-of-structs in the C ABI, so the whole thing is built
 * inside one confined {@link Arena} whose lifetime is exactly the native call. Field offsets come
 * from {@link WmNative} so they can be checked against {@code bindings.h}.
 *
 * <p>Bindings are written into a {@code DrawCall} as they arrive, and a draw is one call across the
 * ABI carrying the pipeline, the buffers, the parameters and every binding by slot - see {@link #record}.
 * The bind groups themselves live in the native pass and are reused between draws.
 */
public class WgpuRenderPass implements RenderPassBackend {

    private static final MemoryLayout CLEAR_COLOR_LAYOUT =
            MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_FLOAT);

    private final WgpuDevice device;
    private final WgpuCommandEncoder encoder;
    private MemorySegment nativePass;
    private final boolean wantsDepth;
    private final AtomicBoolean closed = new AtomicBoolean();

    /** The colour and depth views this pass was opened with, for the graph pass that may replace it. */
    private final MemorySegment colorView;
    private final MemorySegment depthView;

    /**
     * Set once the render graph has drawn this pass instead of Minecraft.
     *
     * <p>From then on this object is a pass whose native side is gone: the graph recorded its own, with
     * the same attachments, and everything Minecraft goes on to record for this pass is dropped rather
     * than drawn twice. See {@link #takeOverTerrainPass}.
     */
    private boolean graphTerrain;

    /**
     * The scratch buffer every draw of this pass is written into, taken from a per-thread pool.
     *
     * <p>A pass is created and closed many times per frame and the buffer is the same 1.2 KiB every
     * time, so an arena per pass paid for an arena, an allocation and a close on the draw path for
     * memory that was handed straight back. A closed pass returns its buffer to its thread's pool
     * and the next pass takes it back: the arena is created once per render thread and the buffer
     * once per pass that is open at the same time - nested passes included, since each takes its own.
     *
     * <p>A reused buffer is reset as it is taken, because the pass before it left its bindings, its
     * vertex buffer mask and its index buffer behind and those are exactly the fields a pass that
     * binds nothing would otherwise inherit. The arena is never closed: it belongs to the thread, not
     * to the pass.
     */
    private record DrawCallBuffer(Arena arena, MemorySegment segment, Thread owner) {

        private static final ThreadLocal<ArrayDeque<DrawCallBuffer>> POOL =
                ThreadLocal.withInitial(ArrayDeque::new);

        static DrawCallBuffer acquire() {
            Thread owner = Thread.currentThread();
            ArrayDeque<DrawCallBuffer> pool = POOL.get();
            DrawCallBuffer buffer = pool.pollLast();

            if (buffer == null) {
                Arena arena = Arena.ofConfined();
                buffer = new DrawCallBuffer(arena, arena.allocate(WmNative.DRAW_CALL), owner);
            }

            // Every field of a zeroed draw call is the "nothing bound" value: slot kind
            // `DRAW_BINDING_NONE`, a null pipeline, a null index buffer, an empty vertex mask. The
            // previous pass left its own in there, and a pass that binds nothing would inherit them.
            buffer.segment.fill((byte) 0);
            return buffer;
        }

        /**
         * Hands the buffer back to this thread's pool, where the next pass picks it up.
         *
         * <p>A confined arena belongs to the thread that made it, so a pass closed somewhere else
         * leaves its buffer to the garbage collector rather than putting an arena another thread
         * cannot touch into that thread's pool. Blaze3D closes its passes on the render thread, which
         * is the thread that opened them, so this is a guard rather than a path.
         */
        void release() {
            if (owner == Thread.currentThread()) {
                POOL.get().addLast(this);
            }
        }
    }

    /** `struct DrawCall`, written in place per draw. */
    private final DrawCallBuffer drawCallBuffer;
    private final MemorySegment drawCall;

    /**
     * The bindings the pass has been given, by the name Minecraft binds them under.
     * <p>
     * Sticky, the way OpenGL's state is: a draw reuses whatever was bound before it, and a pipeline
     * change re-emits all of it, because a slot means something different under a different plan.
     * Uniforms and texel buffers take one slot each; a combined sampler is a pair and lives in
     * {@link #boundSamplers}, because the plan splits it into a texture slot and a sampler slot.
     */
    private final Map<String, Bound> boundBindings = new HashMap<>();

    /** The combined samplers the pass has been given, by the name Minecraft binds them under. */
    private final Map<String, Sampled> boundSamplers = new HashMap<>();

    /** One remembered binding: what it is, and for a buffer, which slice of it. */
    private record Bound(int kind, MemorySegment resource, long offset, long length) {
        static Bound buffer(MemorySegment resource, long offset, long length) {
            return new Bound(WmNative.DRAW_BINDING_BUFFER, resource, offset, length);
        }

        static Bound texture(MemorySegment resource) {
            return new Bound(WmNative.DRAW_BINDING_TEXTURE, resource, 0L, 0L);
        }

        static Bound sampler(MemorySegment resource) {
            return new Bound(WmNative.DRAW_BINDING_SAMPLER, resource, 0L, 0L);
        }
    }

    /** A combined sampler, remembered as the two bindings the plan splits it into. */
    private record Sampled(Bound texture, Bound sampler) {
    }

    /** The pipeline's binding slots, or null when it could not be resolved. */
    private PlanBindings bindings;

    /**
     * What the current binding table is made of, by slot, as the combination is built from it.
     *
     * <p>A draw hands the native side a *number* for its bindings rather than the bindings
     * themselves: that number is the key its bind groups are cached under, and it turns the per-draw
     * question from a walk over every slot into one integer comparison. One stamp per slot is what
     * that number is minted from - the resource, the length and anything else a bind group is built
     * from, but not the offset of a binding whose offset travels with the draw. Slot `i` is cleared
     * to zero by {@link #clearBindings}, which is the same "nothing here" the draw call itself
     * carries.
     */
    private final long[] slotStamps = new long[WmNative.MAX_DRAW_BINDINGS];

    /** The slots [lastCombo] was minted from, so a run of draws sharing them skips the table. */
    private final long[] lastStamps = new long[WmNative.MAX_DRAW_BINDINGS];

    /** The number the last draw recorded, and how many slots went into it. */
    private int lastCombo;
    private int lastCount = -1;

    /**
     * Whether the pipeline bound now has any binding whose offset travels with the draw.
     *
     * <p>Those offsets are the one thing a combination leaves out, so a plan without one is
     * described completely by its number: its draws leave the binding table behind. Read once per
     * pipeline bind, because the answer is a property of the plan.
     */
    private boolean planHasDynamic;

    /** Kept for diagnostics: an `Animate ...` pass has its target dumped once it has been submitted. */
    private final String label;
    private final MemorySegment nativeColorTexture;

    /** The label of the colour target, for the dumps that recognise a target by its name. */
    private final String targetLabel;

    /**
     * Size of the render area, i.e. of the colour target's own mip level.
     *
     * <p>`getWidth(0)` on a view is that level's width, which is also the extent wgpu gives the
     * pass, so a scissor rectangle can be clamped against it.
     */
    private final int targetWidth;
    private final int targetHeight;

    private MemorySegment activePipeline = MemorySegment.NULL;

    /** The location of the pipeline bound now, for the diagnostics that name one. */
    private String activePipelineName = null;

    private int openDebugGroups;

    public WgpuRenderPass(
            WgpuDevice device,
            WgpuCommandEncoder encoder,
            @NonNull Supplier<String> label,
            @NonNull GpuTextureView colorTexture,
            OptionalInt clearColor,
            GpuTextureView depthTexture,
            OptionalDouble clearDepth) {
        this.device = device;
        this.encoder = encoder;
        this.label = label.get();
        this.nativeColorTexture = ((WgpuTextureView) colorTexture).texture().nativeTexture();
        this.targetLabel = ((WgpuTextureView) colorTexture).texture().getLabel();
        this.targetWidth = colorTexture.getWidth(0);
        this.targetHeight = colorTexture.getHeight(0);
        this.wantsDepth = depthTexture != null;
        this.colorView = nativeViewOf(colorTexture);
        this.depthView = depthTexture == null ? MemorySegment.NULL : nativeViewOf(depthTexture);
        this.drawCallBuffer = DrawCallBuffer.acquire();
        this.drawCall = drawCallBuffer.segment();
        this.nativePass = buildDescriptor(this.label, colorTexture, clearColor, depthTexture, clearDepth);
        describeOnce(this.label, colorTexture, depthTexture);
    }

    /**
     * Says which render target each distinct pass draws into, once per pass label.
     *
     * <p>Diagnostics. "The GUI is missing" is either "the GUI went into a different texture than the
     * one that gets presented" or "something drew over it", and the target the pass names is what
     * tells those apart.
     */
    private static void describeOnce(String label, GpuTextureView color, GpuTextureView depth) {
        if (!Diagnostics.loggingEnabled()) {
            return;
        }
        if (!DESCRIBED.add(label + " -> " + color.getWidth(0) + "x" + color.getHeight(0))) {
            return;
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: pass {} draws into a {}x{} {} texture (depth: {})",
                label,
                color.getWidth(0),
                color.getHeight(0),
                ((WgpuTextureView) color).texture().getFormat(),
                depth == null ? "none" : ((WgpuTextureView) depth).texture().getFormat());
    }

    /**
     * Whether this pass draws into the GUI item atlas, which is dumped as a texture of its own.
     *
     * <p>That atlas is neither uploaded nor given a pass of its own to be recognised by: the items
     * of a frame are rendered into it through the feature renderer, whose passes are named after the
     * render type and shared with the world. "The icons are missing" is either "the atlas is empty"
     * or "the GUI blits the wrong part of it", and this dump is the file that says which - after the
     * pass has closed, because at creation the atlas holds the clear it was made with.
     */
    private boolean drawsIntoTheItemAtlas() {
        return targetLabel != null
                && targetLabel.toLowerCase(java.util.Locale.ROOT).contains("ui items atlas");
    }

    private static final java.util.Set<String> DESCRIBED = java.util.concurrent.ConcurrentHashMap.newKeySet();

    private MemorySegment buildDescriptor(
            String label,
            GpuTextureView colorTexture,
            @NonNull OptionalInt clearColor,
            GpuTextureView depthTexture,
            OptionalDouble clearDepth) {
        try (Arena arena = Arena.ofConfined()) {
            // struct BlazeAttachmentDescriptor_f32x4 { const uint8_t *texture_view; const float (*clear_value)[4]; }
            MemorySegment colorAttachment = arena.allocate(WmNative.ATTACHMENT_F32X4);
            colorAttachment.set(WmNative.PTR, WmNative.FIELD_TEXTURE_VIEW, nativeViewOf(colorTexture));

            MemorySegment clearColorPtr = MemorySegment.NULL;
            if (clearColor.isPresent()) {
                int argb = clearColor.getAsInt();
                clearColorPtr = arena.allocate(CLEAR_COLOR_LAYOUT);
                clearColorPtr.set(ValueLayout.JAVA_FLOAT, 0L, ARGB.red(argb) / 255.0f);
                clearColorPtr.set(ValueLayout.JAVA_FLOAT, 4L, ARGB.green(argb) / 255.0f);
                clearColorPtr.set(ValueLayout.JAVA_FLOAT, 8L, ARGB.blue(argb) / 255.0f);
                clearColorPtr.set(ValueLayout.JAVA_FLOAT, 12L, ARGB.alpha(argb) / 255.0f);
            }
            colorAttachment.set(WmNative.PTR, WmNative.FIELD_CLEAR_VALUE, clearColorPtr);

            // struct RawArray_BlazeAttachmentDescriptor_f32x4 { const struct *contents; uint64_t size; }
            MemorySegment attachments = arena.allocate(WmNative.RAW_ARRAY);
            WmNative.writeRawArray(attachments, 0L, colorAttachment, 1L);

            // struct BlazeAttachmentDescriptor_f64 { const uint8_t *texture_view; const double *clear_value; }
            MemorySegment depthAttachment = MemorySegment.NULL;
            if (depthTexture != null) {
                depthAttachment = arena.allocate(WmNative.ATTACHMENT_F64);
                depthAttachment.set(WmNative.PTR, WmNative.FIELD_TEXTURE_VIEW, nativeViewOf(depthTexture));

                MemorySegment clearDepthPtr = MemorySegment.NULL;
                if (clearDepth.isPresent()) {
                    clearDepthPtr = arena.allocate(ValueLayout.JAVA_DOUBLE);
                    clearDepthPtr.set(ValueLayout.JAVA_DOUBLE, 0L, clearDepth.getAsDouble());
                }
                depthAttachment.set(WmNative.PTR, WmNative.FIELD_CLEAR_VALUE, clearDepthPtr);
            }

            // struct BlazeRenderPassDescriptor { const RawArray *attachments; const BlazeAttachmentDescriptor_f64 *depth_attachment; }
            MemorySegment descriptor = arena.allocate(WmNative.RENDER_PASS_DESCRIPTOR);
            descriptor.set(WmNative.PTR, WmNative.FIELD_TEXTURE_VIEW, attachments);
            descriptor.set(WmNative.PTR, WmNative.FIELD_CLEAR_VALUE, depthAttachment);

            return (MemorySegment) invoke(
                    WmNative.createRenderPass,
                    encoder.nativeEncoder(),
                    descriptor,
                    NativeNames.utf8(label));
        }
    }

    private static @NonNull MemorySegment nativeViewOf(GpuTextureView view) {
        return ((WgpuTextureView) view).nativeView();
    }

    /** {@code invokeExact} declares a checked {@link Throwable}; a failure here is fatal. */
    private static Object invoke(MethodHandle handle, Object... args) {
        try {
            return handle.invokeWithArguments(args);
        } catch (Throwable throwable) {
            throw new IllegalStateException("wgpu native call failed", throwable);
        }
    }

    @Override
    public void pushDebugGroup(@NonNull Supplier<String> label) {
        openDebugGroups++;
    }

    @Override
    public void popDebugGroup() {
        if (openDebugGroups > 0) {
            openDebugGroups--;
        }
    }

    @Override
    public void setPipeline(@NonNull RenderPipeline pipeline) {
        // The variant this pass needs, which the compiled pipeline writes on demand the first time:
        // a pass with a depth attachment and one without cannot share a `wgpu::RenderPipeline`.
        WgpuCompiledRenderPipeline compiled =
                WgpuCompiledRenderPipeline.of(device, pipeline, device.defaultShaderSource(), wantsDepth);
        this.activePipeline = compiled.forDepth(wantsDepth);
        this.activePipelineName = pipeline.getLocation().toString();

        // The slots of the new plan, and everything bound so far re-emitted into them: the same
        // binding name sits in a different slot under a different pipeline, so what was written for
        // the previous one is meaningless here.
        PlanBindings slots = compiled.slotsForJava();
        if (slots == null) {
            throw new IllegalStateException("wgpu: " + pipeline.getLocation()
                    + " has more bindings than the draw call can carry; see the log");
        }
        this.bindings = slots;
        this.planHasDynamic = slots.dynamicSlotCount() > 0;
        clearBindings();
        // Only the names the **new** plan has, and only to keep them, not as a fresh request.
        //
        // The stash is per pass and shared across every pipeline the game sets in it, so a name bound
        // under one pipeline is still in it when the next one is set - and re-emitting it blindly is how
        // `minecraft:pipeline/animate_sprite_interpolate` came to be reported as reading a `Sprite` it
        // does not declare, and `.../animate_sprite_blit` as reading `CurrentSprite` and `NextSprite` it
        // does not declare either. The two pipelines are bound alternately, once per animated sprite, so
        // each reported the other's names: a pair of warnings that says nothing about either, and that
        // buries the one thing this diagnostic exists for - a *requested* name the shader spells
        // differently.
        //
        // Dropping a name the new plan has not got is right rather than merely quiet: the game binds what
        // each pipeline's shader declares, and it binds it after `setPipeline`, so a name that survives a
        // pipeline change is one the previous pipeline's draw needed. `reportUnplanned` is false here
        // because nothing is being requested - see `reportUnplannedBinding`, which the two binding entry
        // points call for the names a caller actually asked for.
        for (Map.Entry<String, Bound> entry : boundBindings.entrySet()) {
            writeBinding(entry.getKey(), entry.getValue(), false);
        }
        for (Map.Entry<String, Sampled> entry : boundSamplers.entrySet()) {
            writeSampled(entry.getKey(), entry.getValue(), false);
        }
        reportPlanOnce(slots);

        // The Rust terrain path: a pass Minecraft has opened for one of the two terrain groups is the
        // one the render graph draws, so this is where it is handed over - the pipeline is what says
        // which pass this is, and it is bound before anything is drawn into it.
        //
        // **Each group is recorded at its own pass, and that is not an optimization.** Minecraft draws
        // its opaque terrain, then its entities and features, then its translucent terrain
        // (`LevelRenderer#addMainPass`); the graph is asked for one group at a time
        // (`render_terrain_pass`'s `translucent` flag) so that the water lands where the game's own water
        // lands. Recording both groups at the *opaque* pass - which is what this did first - draws the
        // water before the entities, and the entities are then drawn on top of it: a mob standing in a
        // lake with no water in front of it at all.
        //
        // Each group's own pipeline is where that group's *meshes* are dropped: the opaque pass now, and
        // the translucent pass when Minecraft opens it. `graphDrawnThisFrame` tells the second visit that
        // the frame is already being drawn by the graph, which is what stops Minecraft's own translucent
        // meshes from being recorded on top of the graph's. `usesOwnTarget` is the one case that refuses
        // the second outright - see `TerrainPass.TRANSLUCENT_TERRAIN`.
        //
        // The last two conditions are the same trade twice: a pass taken away from Minecraft before the
        // graph can draw it, or before the arena holds anything to draw, is a frame with no ground in it
        // rather than a frame drawn another way. The second one is asked per frame, because the arena
        // fills as the world is meshed - and it empties again if the camera leaves everything it held.
        if (!graphTerrain
                && TerrainPass.INSTANCE.isOn()
                && TerrainPass.INSTANCE.replaces(activePipelineName)
                && !depthView.equals(MemorySegment.NULL)
                && TerrainPass.INSTANCE.ready(device.renderer())
                && TerrainPass.INSTANCE.hasGeometry(device.renderer())) {
            if (TerrainPass.INSTANCE.usesOwnTarget(activePipelineName)) {
                // Asked here rather than in the guard, so that it is only said once the graph could have
                // drawn this pass at all: a player who has not taken the terrain over does not need a
                // line about water.
                TerrainPass.INSTANCE.reportOwnTarget();
            } else if (TerrainPass.INSTANCE.isTranslucent(activePipelineName)) {
                // The translucent group's own pass, and it is recorded here - after the entities the game
                // has already drawn into this target, which is the whole reason it is a separate call.
                graphTerrain = true;
                takeOverTerrainPass(true);

                if (TerrainPass.INSTANCE.noteTranslucentTakenOver()) {
                    dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                            "wgpu: the render graph is drawing the translucent terrain too, so water "
                                    + "and glass come out of the arena; Minecraft's own meshes for "
                                    + "that layer are skipped");
                }
            } else {
                takeOverTerrainPass(false);
            }
        }
    }

    /**
     * Draws this pass with the render graph instead of with Minecraft's own meshes.
     *
     * <p>Minecraft's pass is finished first, because wgpu allows one pass to be recording at a time and
     * the graph opens its own: nothing has been drawn into it - the pipeline that says "this is the
     * solid layer" is the first thing such a pass is given - and a finished pass keeps whatever it
     * recorded, so this is where the two meet rather than a reordering of the frame.
     *
     * <p>The colour and depth views are the ones this pass was opened with, which is what makes the
     * graph's terrain the terrain of *this* frame: the translucent layer, the entities and everything
     * else Minecraft draws afterwards test against the depth this pass fills. The pass being replaced
     * is the game's opaque group, which draws the solid layer and then the cutout one inside one
     * render pass, so both of those come out of the arena here - see {@code @geo_terrain}.
     *
     * <p>{@code translucent} picks which of the two groups the graph is asked for. It is the group this
     * pass stands in for, and the two are one frame apart: the opaque group is drawn before the game's
     * entities and the translucent group after them, so asking for the wrong one puts water either in
     * front of or behind everything the game draws between them.
     */
    private void takeOverTerrainPass(boolean translucent) {
        invoke(WmNative.dropRenderPass, nativePass);
        nativePass = MemorySegment.NULL;
        graphTerrain = true;

        TerrainPass.INSTANCE.sendCameraMatrices();

        boolean drawn = (boolean) invoke(
                WmNative.renderTerrainPass, device.renderer(), colorView, depthView, translucent);

        // Null on most frames: the pass is taken over every frame of a world, and a line per frame is
        // half a megabyte of log in thirty seconds. It answers when there is something to say - the
        // first frame, a frame that drew nothing, or a change in the arena - and once a second
        // otherwise. See `TerrainPass.describeFrame`.
        String report = TerrainPass.INSTANCE.describeFrame(drawn);

        if (report != null) {
            dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                    "wgpu: {} is drawn by the render graph now{}{}",
                    label,
                    drawn ? "" : "; nothing was drawn, so this frame has no ground in it",
                    report);
        }
    }

    /** Plans already described by [reportPlanOnce]. */
    private static final java.util.Set<String> PLANS_REPORTED = java.util.concurrent.ConcurrentHashMap.newKeySet();

    /**
     * Diagnostics: a pipeline's plan, slot by slot, once.
     * <p>
     * The slot table is what turns a binding *name* into the number the shader's
     * `layout(binding = N)` was written with, and the two are built from the same plan - which is
     * exactly why they have to be checked together rather than trusted: a name that lands one slot
     * away from its declaration samples the texture the neighbouring binding holds, and the symptom
     * of that is a model wearing the wrong texture, deterministically, on every draw.
     */
    private static void reportPlanOnce(PlanBindings plan) {
        if (!Diagnostics.loggingEnabled() || !PLANS_REPORTED.add(plan.label)) {
            return;
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: {} binds {} slot(s): {}", plan.label, plan.getCount(), plan.bindableNames());
    }

    /**
     * Diagnostics: the slots the plan declares that this draw leaves empty, named by the binding they
     * belong to.
     * <p>
     * A pipeline change clears every slot and writes back what was bound so far, so a slot that is
     * still `DRAW_BINDING_NONE` when the draw is recorded is a binding this pipeline has and the pass
     * does not - the
     * same name sitting in a different slot, a name the new plan spells differently, or one that was
     * never bound at all. Which of those it is decides whether the draw that follows is fine, so
     * none of them is left unsaid: with the binding-resolution switch on, every empty slot is
     * reported with the pipeline it belongs to and the name that slot holds.
     * <p>
     * Off by default because a pass binds *after* it sets its pipeline, so the slots of a pipeline
     * that has just been bound are normally empty at this point - the interesting ones are the slots
     * that are still empty by the time the pass draws, which is what the binding log is turned on to
     * look at.
     */
    private void reportEmptySlots() {
        if (bindings == null || !Diagnostics.bindingsEnabled()) {
            return;
        }

        for (int slot = 0; slot < bindings.getCount(); slot++) {
            if (readSlotKind(slot) != WmNative.DRAW_BINDING_NONE) {
                continue;
            }

            if (!EMPTY_SLOTS.add(activePipelineName + "/" + slot + "/" + bindings.nameOf(slot))) {
                continue;
            }

            dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                    "wgpu: {} has nothing bound in slot {} ('{}') after re-binding what this pass "
                            + "held; the plan's {} binding(s) are: {}",
                    activePipelineName,
                    slot,
                    bindings.nameOf(slot),
                    bindings.getCount(),
                    bindings.bindableNames());
        }
    }

    /** Slots already reported by [reportEmptySlots], by pipeline, slot and name. */
    private static final java.util.Set<String> EMPTY_SLOTS = java.util.concurrent.ConcurrentHashMap.newKeySet();

    /** The kind the draw call currently carries for [slot]. */
    private int readSlotKind(int slot) {
        long base = WmNative.DRAW_CALL_BINDINGS + WmNative.elementOffset(WmNative.DRAW_BINDING, slot);
        return drawCall.get(WmNative.INT, base + WmNative.DRAW_BINDING_KIND);
    }

    @Override
    public void bindTexture(@NonNull String name, GpuTextureView textureView, GpuSampler sampler) {
        if (textureView == null || sampler == null) {
            return;
        }

        if (Diagnostics.loggingEnabled() && BOUND.add(label + " " + name + " -> " + ((WgpuTextureView) textureView).texture().getLabel())) {
            // Diagnostics: which texture a sampler actually got - and *every* texture it is given in
            // the same pass. A model wearing another model's texture is either "the shader sampled
            // nothing" or "the shader sampled something else", and the pass label plus the texture
            // label is what tells those apart; the block atlas and the lightmap are both bound to a
            // terrain pass, and swapping them would look exactly like it. Keyed by the texture as
            // well as the name, because "one pass bound Sampler0 to two different skins" is the
            // question that a mob in the wrong skin asks - and the answer says which side mixed them
            // up: this line is the JVM handing the texture over, and a wrong picture after it is the
            // native side's bind group.
            WgpuTexture texture = ((WgpuTextureView) textureView).texture();
            dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                    "wgpu: pass {} binds {} to {} ({}x{}, {})",
                    label,
                    name,
                    texture.getLabel(),
                    texture.getWidth(0),
                    texture.getHeight(0),
                    texture.getFormat());
        }

        // One name, two slots: the plan declares a combined sampler as a texture and a sampler, and
        // this is the pair under the name Minecraft binds it with. Remembered as a pair so a pipeline
        // change can re-emit both halves - the plan's slots are per pipeline, but the pair is not.
        Sampled pair = new Sampled(
                Bound.texture(nativeViewOf(textureView)),
                Bound.sampler(((WgpuSampler) sampler).nativeSampler()));
        boundSamplers.put(name, pair);
        writeSampled(name, pair);

        if (Diagnostics.loggingEnabled()) {
            // Diagnostics: the label of the view handed over, so the per-draw trace can name the
            // pointer a draw call's texture slot holds.
            Diagnostics.noteView(pair.texture().resource().address(), ((WgpuTextureView) textureView).texture().getLabel());
        }
    }

    /** Writes a combined sampler into the two slots its name has in the current plan. */
    private void writeSampled(String name, Sampled pair) {
        writeSampled(name, pair, true);
    }

    /**
     * The same, with the choice of whether a name the plan has not got is worth saying.
     *
     * `requested` is false when the name is being carried over a pipeline change rather than asked for -
     * see {@code setPipeline}, where a name the new plan has not got is dropped in silence because the
     * game rebinds what each pipeline's shader declares.
     */
    private void writeSampled(String name, Sampled pair, boolean requested) {
        int[] slots = bindings == null ? null : bindings.of(name);
        if (slots == null || slots.length == 0) {
            if (requested) {
                reportUnplannedBinding("texture + sampler", name);
            }

            return;
        }

        if (slots.length < 2) {
            if (requested) {
                reportUnpairedSampler(name, slots);
            }

            return;
        }

        writeSlot(slots[0], pair.texture());
        writeSlot(slots[1], pair.sampler());
    }

    /** Diagnostics: each `pass + sampler` pair is reported once. */
    private static final java.util.Set<String> BOUND = java.util.concurrent.ConcurrentHashMap.newKeySet();

    /**
     * Diagnostics: what every texture slot of one draw carries, in slot order.
     * <p>
     * The `pass X binds Sampler0 to Y` line is deduplicated, so it says which textures a pass has
     * *ever* bound - and "every mob wears some other mob's skin" is a question about one draw, not
     * about a pass. This reads the slots the draw call is about to be recorded with, which is the
     * same list the native side walks, in the same order: the native `trace &lt;pipeline&gt; slot '&lt;name&gt;'
     * -&gt; '&lt;label&gt;'` line for the same draw is what says which side mixed the textures up, and the two
     * only line up if both sides print every slot rather than the ones they happened to bind.
     * <p>
     * The name comes from the plan and the label from the registry of views this side has handed
     * over, because the slot holds a pointer and nothing else. Restricted to the pipeline family
     * named by the `wgpu-trace-plan` file, because a line per draw per texture slot is a log that is
     * unusable in a second, and capped for the same reason.
     */
    private void traceDraw(int first, int count, int baseVertex, int instanceCount, boolean indexed) {
        String filter = Diagnostics.tracePlanFilter();
        if (filter == null || activePipelineName == null || !activePipelineName.contains(filter)) {
            return;
        }

        if (!Diagnostics.traceDrawDue()) {
            return;
        }

        StringBuilder line = new StringBuilder();

        for (int slot = 0; slot < bindings.getCount(); slot++) {
            int kind = readSlotKind(slot);
            if (kind != WmNative.DRAW_BINDING_TEXTURE && kind != WmNative.DRAW_BINDING_BUFFER) {
                continue;
            }

            long base = WmNative.DRAW_CALL_BINDINGS + WmNative.elementOffset(WmNative.DRAW_BINDING, slot);
            MemorySegment resource = drawCall.get(WmNative.ADDRESS, base + WmNative.DRAW_BINDING_RESOURCE);

            if (!line.isEmpty()) {
                line.append(", ");
            }

            line.append(bindings.nameOf(slot)).append(" -> ");

            if (kind == WmNative.DRAW_BINDING_TEXTURE) {
                line.append(Diagnostics.viewLabelOf(resource.address()));
            } else {
                // The slice a uniform is read from, which is the number a draw's *transform* comes
                // from: every entity in a batch binds the same buffer and differs only here, so
                // "all the models are in the same place" is answered by this number and nothing else.
                line.append("buffer+")
                        .append(drawCall.get(WmNative.LONG, base + WmNative.DRAW_BINDING_OFFSET))
                        .append("+")
                        .append(drawCall.get(WmNative.LONG, base + WmNative.DRAW_BINDING_LENGTH));
            }
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: trace draw {} [{}] geometry: vertexBuffer0={} indexBuffer={} first={} count={} baseVertex={} instances={} indexed={}",
                activePipelineName, line, vertexBufferDescription(), indexBufferDescription(),
                first, count, baseVertex, instanceCount, indexed);
    }

    /** The vertex buffer slot 0 of this draw, as `address+offset+size`, for the trace. */
    private String vertexBufferDescription() {
        long base = WmNative.DRAW_CALL_VERTEX_BUFFERS;
        MemorySegment buffer = drawCall.get(WmNative.ADDRESS, base);
        long offset = drawCall.get(WmNative.LONG, base + 8L);
        long size = drawCall.get(WmNative.LONG, base + 16L);

        return String.format("0x%x+%d+%d", buffer.address(), offset, size);
    }

    /** The index buffer of this draw, as `address`, for the trace. */
    private @NonNull String indexBufferDescription() {
        MemorySegment buffer = drawCall.get(WmNative.ADDRESS, WmNative.DRAW_CALL_INDEX_BUFFER);
        return String.format("0x%x", buffer.address());
    }

    /** Diagnostics: every binding name that was not in its pipeline's plan, by pipeline and name. */
    private static final java.util.Set<String> UNPLANNED = java.util.concurrent.ConcurrentHashMap.newKeySet();

    /**
     * Diagnostics: a name that is not in the pipeline's plan, reported once per pipeline and name.
     * <p>
     * This used to return in silence, and silence here is the worst possible answer: nothing is
     * written into any slot, so a shader that *does* read this binding reads whatever was there -
     * zeroes, most of the time - and the picture is wrong in a way that looks like a shader bug or a
     * resource bug. The cloud layer was one square above the player's head for exactly this reason,
     * and the log said nothing.
     * <p>
     * A plan is built from what the pipeline's shader declares, so a name that is not in one is
     * usually a name this shader does not use - `RenderSystem#bindDefaultUniforms` binds `Globals`,
     * `Lighting` and `Fog` into every pass, including the ones whose pipeline declares none of them.
     * The interesting case is the other one: the shader declares the binding under a *different
     * spelling*, which is exactly what the shader shim's suffixes do, and then the binding is
     * dropped for a shader that reads it. The plan's own names are printed with the warning because
     * that is the comparison that tells the two cases apart; the binding-resolution switch
     * (`binding_verbosity`) adds the near misses, the lookups that only resolved through a suffix.
     */
    private void reportUnplannedBinding(String kind, String name) {
        if (bindings == null || activePipelineName == null) {
            return;
        }

        if (!UNPLANNED.add(activePipelineName + "/" + name)) {
            return;
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.warn(
                "wgpu: {} was bound a {} under the name '{}', which is not in its binding plan, so "
                        + "nothing was written into a slot. A plan is built from what the shader "
                        + "declares, so this is either a binding the shader does not use (the "
                        + "default uniforms are bound into every pass) or one it declares under a "
                        + "spelling this name is not. The plan's {} binding(s) are: {}",
                activePipelineName,
                kind,
                name,
                bindings.getCount(),
                bindings.bindableNames());
    }

    /**
     * Diagnostics: a combined sampler whose name has only one slot in the plan.
     * <p>
     * The shim splits a sampler into a texture and a sampler slot, and the plan is built from the
     * shader that resulted, so a name with one slot is a plan this side built from a shader that was
     * not shimmed - one half of the pair would be bound and the other left empty, which samples
     * nothing.
     */
    private void reportUnpairedSampler(String name, int[] slots) {
        if (bindings == null || activePipelineName == null) {
            return;
        }

        if (!UNPLANNED.add(activePipelineName + "/" + name + "/unpaired")) {
            return;
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.warn(
                "wgpu: {} was bound a texture and a sampler under '{}', which has {} slot(s) in its "
                        + "plan ({}) and needs two - one for the texture and one for the sampler. "
                        + "The plan's {} binding(s) are: {}",
                activePipelineName,
                name,
                slots.length,
                java.util.Arrays.toString(slots),
                bindings.getCount(),
                bindings.bindableNames());
    }

    @Override
    public void setUniform(@NonNull String name, @NonNull GpuBuffer value) {
        setUniform(name, value.slice());
    }

    @Override
    public void setUniform(@NonNull String name, @NonNull GpuBufferSlice value) {
        // Diagnostics: the offset Minecraft is binding this uniform at, which is the number the
        // dynamic-offset path has to carry through to `set_bind_group` unchanged.
        if (Diagnostics.loggingEnabled()) {
            reportUniformOffset(name, value.offset());
        }

        if (Diagnostics.loggingEnabled() && shouldDumpUniform(name)) {
            dumpUniform(name, value);
        }

        // wgpu wants a buffer binding's size to sit on an alignment, and *which* alignment depends
        // on the binding: a storage buffer's size has to be a multiple of `COPY_BUFFER_ALIGNMENT`
        // (four bytes), a uniform block's the device's uniform alignment. Four satisfies both, and it
        // has to be rounded *down*: rounding up runs past the end of the buffer, which is what the
        // clamp below used to turn back into an unaligned size - a 181,818 byte face buffer became a
        // 181,818 byte storage binding, and wgpu refused to build the cloud bind group at all
        // ("Effective buffer binding size 181818 for storage buffers is expected to align to 4").
        long available = value.buffer().size() - value.offset();
        long wanted = Math.min(value.length(), available);
        long rounded = wanted - Math.floorMod(wanted, 4);
        if (rounded <= 0) {
            rounded = wanted;
        }

        Bound binding = Bound.buffer(
                ((WgpuBuffer) value.buffer()).nativeBuffer(),
                value.offset(),
                Math.clamp(available, 1, rounded));

        boundBindings.put(name, binding);
        writeBinding(name, binding);
        reportCloudBinding(name, (WgpuBuffer) value.buffer(), value.offset(), binding);

        // Remember the texel buffer for the draw-range check below. A texel buffer is the one kind of
        // binding whose *contents* this backend has to have uploaded itself: it is a storage buffer
        // here, filled through a mapped write, and a draw that reads further than the write reached
        // renders whatever the buffer held before.
        if ((value.buffer().usage() & GpuBuffer.USAGE_UNIFORM_TEXEL_BUFFER) != 0) {
            texelBuffer = (WgpuBuffer) value.buffer();
        }
    }

    /** The texel buffer the current bindings read, for [checkTexelReadRange]. */
    private WgpuBuffer texelBuffer = null;

    /**
     * Warns when a draw reads past what was uploaded into the buffer it reads from.
     * <p>
     * A texel buffer is read by `gl_VertexIndex / 4` in the vertex shader, so a draw of `count`
     * indices reads `count / 6` faces of three bytes each - and if the mapped write behind the
     * buffer only reached part of that, the faces past the end are whatever the buffer already held.
     * The result is a cloud layer with faces from an older cell in it, or a layer of stale faces
     * that drift the wrong way, and nothing in the log says so: the upload succeeded, the draw has
     * the count it was given, and the buffer is bound to the right slot.
     * <p>
     * Once a second at most, because this runs per draw.
     */
    private void checkTexelReadRange(int count, boolean indexed) {
        WgpuBuffer faces = texelBuffer;
        if (faces == null || !indexed || count <= 0) {
            return;
        }

        long written = faces.getLastMappedWrite();
        if (written == WgpuBuffer.NEVER_UPLOADED) {
            // Nothing was uploaded through a mapping yet; the buffer was created with its contents.
            return;
        }

        long needed = 3L * (count / 6);
        if (written >= needed || !rangeWarnDue()) {
            return;
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.warn(
                "wgpu: a draw reads {} faces ({} bytes) from {} but only {} bytes were uploaded into it; the rest is whatever the buffer held before",
                count / 6, needed, faces.getLabel(), written);
    }

    /** Whether a [checkTexelReadRange] warning is due, so a per-draw warning cannot flood the log. */
    private static boolean rangeWarnDue() {
        long now = System.nanoTime();
        if (now - lastRangeWarning < 1_000_000_000L) {
            return false;
        }

        lastRangeWarning = now;
        return true;
    }

    private static volatile long lastRangeWarning = 0L;

    /** Diagnostics: each `clouds` binding is reported once, with the slot and the buffer it got. */
    private static final java.util.Set<String> CLOUD_BINDINGS = java.util.concurrent.ConcurrentHashMap.newKeySet();

    /**
     * Diagnostics: which buffer a cloud uniform went into, and into which slot.
     * <p>
     * A binding whose name is not in the pipeline's plan is dropped in silence - `writeBinding`
     * returns without writing anything - so a shader that reads a face buffer nobody bound reads
     * zeroes, and every face of the cloud mesh decodes to cell (0, 0). That is one square of cloud
     * in the whole sky, and it looks exactly like a face buffer full of zeroes, which is why the
     * slot and the buffer have to be checked together rather than guessed at.
     */
    private void reportCloudBinding(String name, WgpuBuffer buffer, long offset, Bound binding) {
        if (!Diagnostics.loggingEnabled() || activePipelineName == null || !activePipelineName.contains("clouds")) {
            return;
        }

        if (!CLOUD_BINDINGS.add(activePipelineName + "/" + name)) {
            return;
        }

        int[] slots = bindings == null ? null : bindings.of(name);
        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: {} bound {} -> slot {} of {} (buffer {} at {}, {} bytes from {}), write {}",
                activePipelineName,
                name,
                slots == null ? "NONE" : java.util.Arrays.toString(slots),
                bindings == null ? "?" : bindings.getCount(),
                buffer.getLabel(),
                binding.resource().address(),
                buffer.size(),
                offset,
                binding.length());
    }

    /** Writes a remembered binding into the slot its name has in the current pipeline's plan. */
    private void writeBinding(String name, Bound binding) {
        writeBinding(name, binding, true);
    }

    /** The same, with the choice of whether a name the plan has not got is worth saying. See [writeSampled]. */
    private void writeBinding(String name, Bound binding, boolean requested) {
        if (binding == null) {
            return;
        }

        int[] slots = bindings == null ? null : bindings.of(name);
        if (slots == null || slots.length == 0) {
            if (requested) {
                reportUnplannedBinding("buffer", name);
            }

            return;
        }

        writeSlot(slots[0], binding);
    }

    /** Writes one binding into one slot of the draw call. */
    private void writeSlot(int slot, Bound binding) {
        if (binding == null || bindings == null || slot < 0 || slot >= bindings.getCount()) {
            return;
        }

        long base = WmNative.DRAW_CALL_BINDINGS + WmNative.elementOffset(WmNative.DRAW_BINDING, slot);
        drawCall.set(WmNative.INT, base + WmNative.DRAW_BINDING_KIND, binding.kind());
        drawCall.set(WmNative.ADDRESS, base + WmNative.DRAW_BINDING_RESOURCE, binding.resource());
        drawCall.set(WmNative.LONG, base + WmNative.DRAW_BINDING_OFFSET, binding.offset());
        drawCall.set(WmNative.LONG, base + WmNative.DRAW_BINDING_LENGTH, binding.length());

        slotStamps[slot] = stampOf(binding, bindings.dynamicSlots[slot]);
    }

    /** Marks every binding slot empty, which a pipeline change has to do before re-emitting. */
    private void clearBindings() {
        if (bindings == null) {
            return;
        }

        for (int slot = 0; slot < bindings.getCount(); slot++) {
            long base = WmNative.DRAW_CALL_BINDINGS + WmNative.elementOffset(WmNative.DRAW_BINDING, slot);
            drawCall.set(WmNative.INT, base + WmNative.DRAW_BINDING_KIND, WmNative.DRAW_BINDING_NONE);
            drawCall.set(WmNative.ADDRESS, base + WmNative.DRAW_BINDING_RESOURCE, MemorySegment.NULL);
            drawCall.set(WmNative.LONG, base + WmNative.DRAW_BINDING_OFFSET, 0L);
            drawCall.set(WmNative.LONG, base + WmNative.DRAW_BINDING_LENGTH, 0L);

            slotStamps[slot] = 0L;
        }

        // The slots are not the ones the last number was minted from any more, and the ones being
        // written now belong to the plan that has just been bound.
        lastCount = -1;
    }

    /** `0x9E3779B97F4A7C15`, the odd constant the native side's own fold multiplies by. */
    private static final long STAMP_MIX = 0x9E3779B97F4A7C15L;

    /**
     * What one slot contributes to the combination, which is everything a bind group is built from.
     *
     * <p>The resource and the length are in it, and the offset too unless the binding is one whose
     * offset travels with the draw - those are handed over per draw instead, and numbering them apart
     * would give every draw of a run its own bind group. The kind is in it because the slots of one
     * plan are not all the same thing: swapping a texture and a sampler between two slots is a
     * different set of bindings even though the two addresses are the same two addresses.
     *
     * <p>This is a mix rather than a hash of the whole table: a stamp is compared, never trusted, so
     * two that collide only cost the table having one entry too many.
     */
    private static long stampOf(Bound binding, boolean offsetTravels) {
        long stamp = binding.kind();
        stamp = (stamp ^ binding.resource().address()) * STAMP_MIX;
        stamp = (stamp ^ (offsetTravels ? 0L : binding.offset())) * STAMP_MIX;
        return (stamp ^ binding.length()) * STAMP_MIX;
    }

    /**
     * Diagnostics: reads a uniform's data back and logs it as floats.
     *
     * <p>A world that is drawn in one flat colour is a shader reading a uniform that never arrived,
     * and the values are the only thing that says which one: the terrain shader's fog distances and
     * colour decide whether anything in front of the camera survives to be drawn, and its section
     * matrix decides where it lands. The readback goes through {@code read_buffer}, which allocates
     * a staging buffer, submits a command buffer and waits for the GPU - so it is rationed twice
     * over, once by name and once by a readbacks-per-second budget.
     */
    private void dumpUniform(String name, @NonNull GpuBufferSlice value) {
        int length = (int) Math.min(value.length(), 256);

        // Closed rather than left to the GC: one of these was leaking a confined arena per uniform
        // bind, and a bind can happen thousands of times per resource reload.
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment staging = arena.allocate(length);

            Boolean filled = (Boolean) invoke(
                    WmNative.readBuffer,
                    device.renderer(),
                    ((WgpuBuffer) value.buffer()).nativeBuffer(),
                    value.offset(),
                    (long) length,
                    staging);

            if (filled == null || !filled) {
                if (UNIFORM_READBACK_FAILED.add(name)) {
                    dev.birb.wgpu.WgpuMcMod.LOGGER.warn(
                            "wgpu: could not read uniform {} back; it is one Rust will not map",
                            name);
                }
                return;
            }

            // Both spellings, because a std140 block mixes them: `Globals.CameraBlockPos` and
            // `ChunkSection.TextureSize` are integers, and reading them as floats prints zero.
            StringBuilder floats = new StringBuilder();
            StringBuilder ints = new StringBuilder();
            for (int i = 0; i + 4 <= length; i += 4) {
                floats.append(String.format(java.util.Locale.ROOT, "%.4g ", staging.get(ValueLayout.JAVA_FLOAT, i)));
                ints.append(staging.get(ValueLayout.JAVA_INT, i)).append(' ');
            }

            dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                    "wgpu: uniform {} at {} ({} bytes) in pass '{}'\n  floats: {}\n  ints:   {}",
                    name, value.offset(), length, label, floats.toString().trim(), ints.toString().trim());
        }
    }

    /** Diagnostics: each uniform name is reported once, and these again whenever they change. */
    private static final java.util.Set<String> UNIFORMS_REPORTED = java.util.concurrent.ConcurrentHashMap.newKeySet();

    /** Diagnostics: the last offset each uniform was bound at, so a move can be reported. */
    private static final java.util.Map<String, Long> UNIFORM_OFFSETS =
            new java.util.concurrent.ConcurrentHashMap<>();

    private static volatile long OFFSETS_REPORTED_AT = 0L;

    /** Diagnostics: reports a uniform whose offset has moved, at most once a second overall. */
    private static void reportUniformOffset(String name, long offset) {
        Long previous = UNIFORM_OFFSETS.put(name, offset);
        if (previous != null && previous == offset) {
            return;
        }

        long now = System.nanoTime();
        if (now - OFFSETS_REPORTED_AT < 1_000_000_000L) {
            return;
        }
        OFFSETS_REPORTED_AT = now;

        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: uniform {} is bound at offset {}", name, offset);
    }

    /**
     * Whether to report a uniform now.
     *
     * <p>Most uniforms are only interesting the first time they are bound. The ones that decide
     * what a world looks like are not: the fog distances are meaningless on the title screen (all
     * of them are {@code FLT_MAX}, i.e. fog off) and only say anything once a level is loaded, and
     * the cloud block is rebuilt every frame. Those are reported again, at most once a second, and
     * the rest stay once per name.
     *
     * <p>Keyed by name and <em>not</em> by offset, which is what an earlier version did: Minecraft
     * binds one uniform per sprite while it animates an atlas, each at its own offset, so an offset
     * in the key turned every sprite's bind into a fresh readback - 4722 of them for the blocks
     * atlas alone, each allocating, submitting and waiting. That is what took the game's memory
     * into the tens of gigabytes during a resource reload.
     */
    private static boolean shouldDumpUniform(String name) {
        if (!UNIFORMS_REPORTED.add(name)) {
            if (!name.startsWith("Fog") && !name.startsWith("Cloud")) {
                return false;
            }

            long now = System.nanoTime();
            Long last = UNIFORM_REPORTED_AT.get(name);
            if (last != null && now - last < 1_000_000_000L) {
                return false;
            }
            UNIFORM_REPORTED_AT.put(name, now);
        }

        // The budget is the backstop: a name nobody expected to be bound in a loop cannot get past
        // it, whatever the key is.
        return takeReadbackBudget();
    }

    /** How many readbacks a second the diagnostics may cost, whatever asks for them. */
    private static final int READBACKS_PER_SECOND = 4;

    private static boolean takeReadbackBudget() {
        long second = System.nanoTime() / 1_000_000_000L;

        synchronized (READBACK_BUDGET) {
            if (READBACK_BUDGET[0] != second) {
                READBACK_BUDGET[0] = second;
                READBACK_BUDGET[1] = 0;
            }

            if (READBACK_BUDGET[1] >= READBACKS_PER_SECOND) {
                if (READBACK_EXHAUSTED != second) {
                    READBACK_EXHAUSTED = second;
                    dev.birb.wgpu.WgpuMcMod.LOGGER.warn(
                            "wgpu: the uniform readbacks hit their budget of {} a second; the rest of this second's uniforms go unreported",
                            READBACKS_PER_SECOND);
                }
                return false;
            }

            READBACK_BUDGET[1]++;
            return true;
        }
    }

    private static final long[] READBACK_BUDGET = {0L, 0L};

    /**
     * The last second the budget was reported for.
     * <p>
     * A field rather than a set of seconds: the set grew by one entry per second for as long as the
     * game ran, which is a leak whose whole purpose was to log a warning.
     */
    private static volatile long READBACK_EXHAUSTED = -1L;

    private static final java.util.Map<String, Long> UNIFORM_REPORTED_AT =
            new java.util.concurrent.ConcurrentHashMap<>();

    /** Uniforms whose readback was refused, so the warning is logged once each. */
    private static final java.util.Set<String> UNIFORM_READBACK_FAILED =
            java.util.concurrent.ConcurrentHashMap.newKeySet();

    @Override
    public void setViewport(int x, int y, int width, int height) {
        if (Diagnostics.loggingEnabled() && VIEWPORTS.add(x + "," + y + "," + width + "," + height)) {
            dev.birb.wgpu.WgpuMcMod.LOGGER.info("wgpu: viewport {} {} {} {}", x, y, width, height);
        }
    }

    private static final java.util.Set<String> VIEWPORTS = java.util.concurrent.ConcurrentHashMap.newKeySet();

    /**
     * Restricts drawing to [x, y, width, height] of the colour target.
     *
     * <p>The rectangle arrives in OpenGL's window coordinates, measured from the target's origin
     * row. That is the row this backend renders into - the clip-space y negation in
     * {@code preprocessing::EmulateGlClipSpace} is what makes it so - and the rectangle is
     * therefore forwarded as it is. The two callers agree on the convention: {@code GuiRenderer}
     * builds its rectangle as {@code windowHeight - bottom * guiScale}, and {@code GuiItemAtlas}
     * renders items with {@code textureSize - bottom}.
     *
     * <p>What is <em>not</em> forwarded as it is, is the rectangle itself: OpenGL clamps a scissor
     * box that reaches outside the framebuffer, wgpu rejects it, and Minecraft's caller does not
     * clamp (`GuiGraphicsExtractor` hands over whole rectangles from screen space, which a scaled
     * GUI can push past the edge). So the intersection with the target is computed here.
     */
    @Override
    public void enableScissor(int x, int y, int width, int height) {
        if (graphTerrain) {
            // The graph's pass has no scissor to set: the solid layer is drawn as one range of sections,
            // and the pass Minecraft would have set this on is gone.
            return;
        }

        // OpenGL's scissor box is [x, x + width) x [y, y + height), and an empty box draws nothing;
        // wgpu accepts a zero-sized one, so an empty intersection needs no special case.
        int left = Math.max(0, x);
        int top = Math.max(0, y);
        int right = Math.min(targetWidth, x + Math.max(0, width));
        int bottom = Math.min(targetHeight, y + Math.max(0, height));

        invoke(
                WmNative.setScissorRect,
                nativePass,
                left,
                top,
                Math.max(0, right - left),
                Math.max(0, bottom - top));

        if (Diagnostics.loggingEnabled() && SCISSORS.add(x + "," + y + "," + width + "," + height)) {
            dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                    "wgpu: scissor {} {} {} {} -> {} {} {} {} of {}x{}",
                    x, y, width, height, left, top, Math.max(0, right - left), Math.max(0, bottom - top), targetWidth, targetHeight);
        }
    }

    private static final java.util.Set<String> SCISSORS = java.util.concurrent.ConcurrentHashMap.newKeySet();

    @Override
    public void disableScissor() {
        if (graphTerrain) {
            return;
        }

        // wgpu's scissor is per draw call and persists inside a pass, exactly like OpenGL's, so
        // turning it off has to put the whole target back rather than leave the last box in place.
        invoke(WmNative.setScissorRect, nativePass, 0, 0, targetWidth, targetHeight);
    }

    @Override
    public void setVertexBuffer(int slot, @NonNull GpuBuffer vertexBuffer) {
        WgpuBuffer buffer = (WgpuBuffer) vertexBuffer;
        long base = WmNative.DRAW_CALL_VERTEX_BUFFERS + WmNative.elementOffset(WmNative.DRAW_VERTEX_BUFFER, slot);

        drawCall.set(WmNative.ADDRESS, base, buffer.nativeBuffer());
        drawCall.set(WmNative.LONG, base + 8L, 0L);
        drawCall.set(WmNative.LONG, base + 16L, buffer.size());

        int mask = drawCall.get(WmNative.INT, WmNative.DRAW_CALL_VERTEX_BUFFER_MASK);
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_VERTEX_BUFFER_MASK, mask | (1 << slot));
    }

    @Override
    public void setIndexBuffer(@NonNull GpuBuffer indexBuffer, VertexFormat.@NonNull IndexType indexType) {
        drawCall.set(WmNative.ADDRESS, WmNative.DRAW_CALL_INDEX_BUFFER, ((WgpuBuffer) indexBuffer).nativeBuffer());
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_INDEX_FORMAT,
                indexType == VertexFormat.IndexType.INT
                        ? WmNative.INDEX_FORMAT_UINT32
                        : WmNative.INDEX_FORMAT_UINT16);

        reportCloudIndexBuffer(((WgpuBuffer) indexBuffer), indexType);
    }

    /**
     * Diagnostics: the first indices of the buffer the clouds are drawn with, read back from the GPU.
     * <p>
     * The cloud vertex shader has no vertex buffer at all: it derives the face it draws from
     * `gl_VertexIndex / 4`, which for an indexed draw is the value in the index buffer. An index
     * buffer full of zeroes therefore draws the *first* face ten thousand times - one square of
     * cloud where the layer should be - and it is indistinguishable, in a screenshot, from a face
     * buffer full of zeroes. The counts cannot tell the two apart either: `drawIndexed` carries the
     * count it was given whether or not the buffer behind it has anything in it.
     */
    private void reportCloudIndexBuffer(WgpuBuffer indexBuffer, VertexFormat.IndexType indexType) {
        if (!Diagnostics.loggingEnabled()
                || activePipelineName == null
                || !activePipelineName.contains("clouds")
                || !CLOUD_BINDINGS.add(activePipelineName + "/index")) {
            return;
        }

        try (Arena arena = Arena.ofConfined()) {
            MemorySegment out = arena.allocate(32);
            boolean read;
            try {
                read = (boolean) WmNative.readBuffer.invokeExact(
                        device.renderer(), indexBuffer.nativeBuffer(), 0L, 32L, out);
            } catch (Throwable error) {
                dev.birb.wgpu.WgpuMcMod.LOGGER.warn("wgpu: the cloud index buffer could not be read back: {}", error);
                return;
            }

            if (!read) {
                dev.birb.wgpu.WgpuMcMod.LOGGER.warn("wgpu: could not read the cloud index buffer back");
                return;
            }

            boolean wide = indexType == VertexFormat.IndexType.INT;
            StringBuilder indices = new StringBuilder();
            for (int i = 0; i < (wide ? 8 : 16); i++) {
                long value = wide
                        ? out.get(WmNative.INT, i * 4L)
                        : out.get(ValueLayout.JAVA_SHORT_UNALIGNED, i * 2L) & 0xFFFFL;
                indices.append(value).append(' ');
            }

            dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                    "wgpu: the cloud index buffer ({}, {} bytes) starts with {}",
                    indexType, indexBuffer.size(), indices.toString().trim());
        }
    }

    @Override
    public void drawIndexed(int baseVertex, int firstIndex, int indexCount, int instanceCount) {
        record(firstIndex, indexCount, baseVertex, instanceCount, true);
    }

    @Override
    public void draw(int firstVertex, int vertexCount) {
        record(firstVertex, vertexCount, 0, 1, false);
    }

    /**
     * Fills the draw call's parameters and records it, which is the whole per-draw ABI.
     * <p>
     * The bindings are already in the buffer - they were written as they were bound - so a draw adds
     * the parameters and the count, and `draw_call` does the rest: the pipeline if it changed, the
     * vertex and index buffers, the bind groups (looked up or built once and then reused by the
     * pass) and finally the draw itself.
     */
    private void record(int first, int count, int baseVertex, int instanceCount, boolean indexed) {
        if (graphTerrain) {
            // The graph drew this pass's terrain, from the meshes the Rust baker built; Minecraft's own
            // meshes for the same layer are what it replaced, so recording them would draw the world
            // twice.
            return;
        }

        if (activePipeline.equals(MemorySegment.NULL) || bindings == null) {
            // Blaze3D never draws without a pipeline; if it ever does, the call would dereference a
            // null pipeline inside wgpu.
            dev.birb.wgpu.WgpuMcMod.LOGGER.error("wgpu: a draw arrived with no pipeline bound");
            return;
        }

        writeTemplate(first, count, baseVertex, instanceCount, indexed, firstInstance);

        reportCloudDraw(first, count, baseVertex, indexed);
        checkTexelReadRange(count, indexed);
        traceDraw(first, count, baseVertex, instanceCount, indexed);

        // Diagnostics: a slot the plan declares that this draw leaves empty. Reported here rather than
        // when the pipeline was set, because a pass sets its pipeline and then binds - so every slot
        // is empty at that moment and the report said nothing but "the pipeline was just set", ten
        // lines per pass. A draw is where an empty slot is a real one: the native side refuses to
        // build a bind group from it, and this names the culprit before that happens.
        reportEmptySlots();

        boolean recorded = (boolean) invoke(WmNative.drawCall, device.renderer(), nativePass, drawCall);

        if (!recorded) {
            // The native side does not know this combination and was not given the bindings to build
            // it from - which is a draw of a plan whose number was supposed to say everything, on the
            // first draw to use that number, or one whose bind groups it has since dropped. It refuses
            // rather than binding whatever it bound last, so the table goes with the draw this time
            // and the same call is made again: the pipeline and the vertex buffers it recorded before
            // it looked at the bindings are the same ones, so re-recording them changes nothing.
            drawCall.set(WmNative.INT, WmNative.DRAW_CALL_BINDINGS_PRESENT, 1);
            recorded = (boolean) invoke(WmNative.drawCall, device.renderer(), nativePass, drawCall);
            reportRefusedDraw(recorded);
        }

        if (!recorded) {
            // Unreachable while the native side only refuses a draw that left its bindings behind. If
            // it ever happens, the picture is missing draws rather than wrong ones.
            dev.birb.wgpu.WgpuMcMod.LOGGER.error(
                    "wgpu: the native side refused a draw of {} that carried its bindings",
                    activePipelineName);
        }
    }

    /**
     * Writes the state a draw is recorded with into the reused draw call, without recording a draw.
     *
     * <p>It exists for {@link #batch}. A batched call carries **one** draw call for a whole run, and it
     * has to carry that run's *current* state: the pipeline, the buffers, the bindings, and the number
     * the native side keys their bind groups by. That number is minted from the slots as they are bound
     * and only a recorded draw used to mint it - so a batch that left the field where the last recorded
     * draw put it handed the native side the identity of somebody else's bindings, and the native side
     * answers that by refusing the draw. It is why one terrain layer batched and the others did not,
     * until this was written: a refusal there costs the batching and never the picture.
     */
    private void writeTemplate(
            int first, int count, int baseVertex, int instanceCount, boolean indexed, int instance) {
        drawCall.set(WmNative.ADDRESS, WmNative.DRAW_CALL_PIPELINE, activePipeline);
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_FIRST, first);
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_COUNT, count);
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_BASE_VERTEX, baseVertex);
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_INSTANCE_COUNT, instanceCount);
        // The draw's own record number, which is what a shader with per-draw data reads its block at.
        // Zero for every pipeline whose draws carry none, and set by `replay`, because Blaze3D's draw
        // calls have no such parameter. See `DRAW_CALL_FIRST_INSTANCE`.
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_FIRST_INSTANCE, instance);
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_INDEXED, indexed ? 1 : 0);
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_BINDINGS_LEN, bindings.getCount());

        // The combination this draw's bindings make up, and whether the table in the buffer carries
        // what it left out. The number is the identity of the bind groups - it is what the native
        // side keys them by - so it is minted from the slots as they are bound, and a draw whose
        // slots are the ones the last draw of this pass had reuses its number without a lookup. Zero
        // means "work the identity out from the table", which is what the native side did before
        // combinations existed; numbering them is what turns that per-draw walk into one integer
        // comparison, and it is done where the bindings are known - here.
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_COMBO, combination());

        // The table is always the one the number was built from - the slots are written as they are
        // bound - so what this really says is whether the number is the *whole* identity: a plan whose
        // bindings all bake their offsets has none that travels with the draw, and its draws are then
        // answered without the table being read at all.
        drawCall.set(WmNative.INT, WmNative.DRAW_CALL_BINDINGS_PRESENT, planHasDynamic ? 1 : 0);
    }

    /** Refused draws already reported by [reportRefusedDraw], by pipeline. */
    private static final java.util.Set<String> REFUSED_DRAWS =
            java.util.concurrent.ConcurrentHashMap.newKeySet();

    /**
     * Diagnostics: a draw the native side asked to see the bindings of, once per pipeline.
     *
     * <p>Expected once per combination a plan of that kind is first drawn with, and again whenever
     * the native side has dropped the bind groups it built - so a line per draw here would mean the
     * numbers are changing per draw, which is the one way this can be slower than the walk it
     * replaced rather than faster.
     */
    private void reportRefusedDraw(boolean recorded) {
        if (!Diagnostics.loggingEnabled() || !REFUSED_DRAWS.add(activePipelineName)) {
            return;
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: {} asked for the bindings of a draw the native side did not know ({} after "
                        + "sending them); its bind groups are rebuilt once per combination",
                activePipelineName,
                recorded ? "recorded" : "still refused");
    }

    /** The number the current binding table is known by, minting one if it has changed. */
    private int combination() {
        int count = bindings.getCount();

        if (count == lastCount && Arrays.equals(lastStamps, 0, count, slotStamps, 0, count)) {
            return lastCombo;
        }

        int combo = bindings.combinationOf(slotStamps, count);
        System.arraycopy(slotStamps, 0, lastStamps, 0, count);
        lastCount = count;
        lastCombo = combo;

        return combo;
    }

    /**
     * Diagnostics: what the clouds are drawn with, once a second.
     * <p>
     * `CloudRenderer` turns its face buffer into `drawIndexed(0, 0, 6 * quadCount, 1)`, so the index
     * count says how many faces the mesh actually has - and the vertex shader derives the face it
     * reads from `gl_VertexIndex / 4`, which makes a wrong or empty index buffer look exactly like a
     * wrong face buffer: every quad reads the same face, and the sky has one square of cloud in it.
     * <p>
     * Once a second and not once per pass: the pass is rebuilt every frame, so a field on it would
     * report every frame, which is a line a frame and a megabyte a minute of the same sentence.
     */
    private void reportCloudDraw(int first, int count, int baseVertex, boolean indexed) {
        if (!Diagnostics.loggingEnabled() || activePipelineName == null) {
            return;
        }

        if (!activePipelineName.contains("clouds") || !cloudDrawDue()) {
            return;
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: {} drawn with {} indices{} from {} (base vertex {})",
                activePipelineName, count, indexed ? "" : " and no index buffer", first, baseVertex);
    }

    /** Whether a [reportCloudDraw] line is due. */
    private static boolean cloudDrawDue() {
        long now = System.nanoTime();
        if (now - lastCloudDraw < 1_000_000_000L) {
            return false;
        }

        lastCloudDraw = now;
        return true;
    }

    private static volatile long lastCloudDraw = 0L;

    /**
     * Draws a multi-draw: the runs that can be one indirect call are, and everything else is replayed
     * one draw at a time.
     *
     * <p>Minecraft hands a whole layer's sections to one call of this - thousands of draws a frame in a
     * handful of calls, all of them the same setup with different indices - so a run of them becomes
     * one `multi_draw_indexed_indirect`. See {@link #batch} for what a run is, and {@link #replay} for
     * the draws that are still one call each.
     *
     * <p>**The per-draw uniform is what makes this more than bookkeeping.** A draw's own block - the
     * terrain pass' `ChunkSection`: the section's position, its fade and the atlas size - arrives as a
     * slice of a uniform ring buffer, one slice per section, and an indirect record has no room for a
     * per-draw binding offset. So a slot the plan marks as per-draw storage is bound *whole* and the
     * number of the slice the draw named travels as its first instance, which is where the shader
     * reads the block: see {@link #captureUniformUploads}. A draw that uploads anything else has no
     * per-draw channel and is replayed.
     */
    @Override
    public <T> void drawMultipleIndexed(
            @NonNull Collection<RenderPass.Draw<T>> draws,
            GpuBuffer defaultIndexBuffer,
            VertexFormat.IndexType defaultIndexType,
            @NonNull Collection<String> dynamicUniforms,
            @NonNull T uniformArgument) {
        if (Diagnostics.loggingEnabled()) {
            countMultiDraw(draws, defaultIndexBuffer, defaultIndexType);
        }

        // **The run being collected for one batched call**, if any draw of this call can be batched at
        // all. A local rather than a field because `T` is the caller's: a `Draw<T>`'s uniform uploader
        // takes a `T`, and only this method knows what that is. [runInstances] is its parallel array,
        // because a boxed number per section per frame is an allocation this path exists to remove.
        List<RenderPass.Draw<T>> run = null;
        GpuBuffer runVertex = null;
        GpuBuffer runIndex = null;
        VertexFormat.IndexType runType = null;
        int runSlot = -1;
        boolean runInstanced = false;

        // **Read once per call, and never inside the loop below.** The switch is the renderer's own
        // `section_indirect` setting rather than a marker file now, but the loop still runs thousands of
        // times a frame and all that has to be true is that the answer does not change within one call.
        boolean batching = batching();

        for (RenderPass.Draw<T> draw : draws) {
            GpuBuffer indexBuffer = draw.indexBuffer() != null ? draw.indexBuffer() : defaultIndexBuffer;
            VertexFormat.IndexType indexType =
                    draw.indexType() != null ? draw.indexType() : defaultIndexType;
            if (indexType == null) {
                indexType = VertexFormat.IndexType.SHORT;
            }

            // The draw's own uploads, taken here: they are the one thing a caller can change between
            // the draws of one call, so they decide both what the run is and whether it can be one.
            captureUniformUploads(draw, uniformArgument);

            boolean batchable =
                    batching && indexBuffer != null && draw.indexCount() > 0 && !capturedForeign;
            boolean sameRun = run != null
                    && runVertex == draw.vertexBuffer()
                    && runIndex == indexBuffer
                    && runType == indexType
                    && runSlot == draw.slot()
                    && runInstanced == (capturedInstance != NO_INSTANCE);

            if (batchable && sameRun) {
                run.add(draw);
                addRunInstance(capturedInstance);
                continue;
            }

            if (run != null) {
                replayRun(run, runIndex, runType);
                run = null;
            }

            if (batchable) {
                run = new ArrayList<>();
                run.add(draw);
                runSize = 0;
                addRunInstance(capturedInstance);
                runVertex = draw.vertexBuffer();
                runIndex = indexBuffer;
                runType = indexType;
                runSlot = draw.slot();
                runInstanced = capturedInstance != NO_INSTANCE;
            } else if (indexBuffer != null) {
                replay(draw, indexBuffer, indexType, capturedInstance);
            }
        }

        if (run != null) {
            replayRun(run, runIndex, runType);
        }
    }

    /** The record number of the draw being captured, or [NO_INSTANCE] when it reads no per-draw data. */
    private int capturedInstance;
    /** Whether the draw being captured uploaded anything that is not per-draw storage data. */
    private boolean capturedForeign;

    /** What [capturedInstance] holds for a draw whose pipeline reads no per-draw data. */
    private static final int NO_INSTANCE = -1;

    /** The record numbers of the run being collected, parallel to its list of draws. */
    private int[] runInstances = new int[64];

    /** How many of [runInstances] the run being collected has filled. */
    private int runSize;

    private void addRunInstance(int instance) {
        if (runSize == runInstances.length) {
            runInstances = Arrays.copyOf(runInstances, runSize * 2);
        }

        runInstances[runSize++] = instance;
    }

    /** The setting that turns the batched draw path off, for a run that wants the other path. */
    private static final String SECTION_INDIRECT = "section_indirect";

    /**
     * Whether the batched draw path may be used, which is the renderer's own `section_indirect` switch.
     *
     * <p>**A setting, not a marker file** - and the marker this replaced was the wrong shape for two
     * reasons that are worth keeping written down. A setting is the switch a player can find: it is in
     * the schema, on the options screen under Debug, persisted in `config/wgpu-mc-renderer.json`, and
     * it costs a hash lookup to read. A marker file is a filesystem call, and reading it *per draw* is
     * what took a frame from 120 fps to 6.5 on a machine whose antivirus filters every open (measured:
     * `Files.exists` on an absent marker, 32.6 µs).
     *
     * <p>The other half of the comparison this exists for - the camera - is `pin_camera`, which is the
     * setting next to it. Two runs with the same pinned camera and this switch set either way give the
     * A/B; nothing has to be created while the game runs.
     */
    private static boolean batching() {
        Boolean asked = RendererSettings.bool(SECTION_INDIRECT);

        // The renderer answers for its own schema; before it has said anything - the first frames of a
        // launch - the path that has been measured is the one to take.
        return asked == null || asked;
    }

    /** Whether a marker file of this name exists in the run directory or the one above it. */
    private static boolean markerPresent(String name) {
        return java.nio.file.Files.exists(java.nio.file.Path.of(name))
                || java.nio.file.Files.exists(java.nio.file.Path.of("..", name));
    }

    /**
     * Runs one draw's own uniform uploads, and sorts what they bind into the two things the draw path
     * needs: the bindings themselves, and - for a slot the plan marks as per-draw storage - the record
     * number the draw reads.
     *
     * <p>This is where a per-draw uniform stops being a per-draw *binding*. A storage slot is bound
     * whole, at offset zero, so that every draw of a batched run shares one bind group; the slice
     * Minecraft named for this draw becomes its first instance instead, which is what
     * {@link #captureUniform} works out.
     *
     * <p>Anything else the draw uploads is bound exactly as it always was, and sets {@link
     * #capturedForeign}: its offset travels with the draw, an indirect record has nowhere to put it,
     * and so the run it lands in is replayed one draw at a time. The uploader runs here and once - a
     * fallback must not run it a second time, because nothing promises it is idempotent.
     */
    private <T> void captureUniformUploads(RenderPass.Draw<T> draw, T uniformArgument) {
        capturedInstance = NO_INSTANCE;
        capturedForeign = false;

        if (draw.uniformUploaderConsumer() != null) {
            draw.uniformUploaderConsumer().accept(uniformArgument, this::captureUniform);
        }
    }

    /** One uniform a draw uploaded. See [captureUniformUploads]. */
    private void captureUniform(String name, GpuBufferSlice value) {
        int slot = perDrawStorageSlot(name);

        if (slot < 0) {
            setUniform(name, value);
            capturedForeign = true;
            return;
        }

        // **The whole buffer, once per draw** - the offsets are all zero, so every draw of a run binds
        // the same thing and the one bind group serves them all. The number of the slice this draw
        // named is what the shader will index it by: the ring those blocks come from advances by one
        // whole block per record, so the slice's offset over its own length is the record it is, and
        // nothing here has to ask a device for a stride.
        GpuBuffer buffer = value.buffer();
        boundBindings.put(name, Bound.buffer(((WgpuBuffer) buffer).nativeBuffer(), 0L, buffer.size()));
        writeSlot(slot, boundBindings.get(name));

        capturedInstance = value.length() > 0L ? (int) (value.offset() / value.length()) : 0;
    }

    /** The slot `name` is, when the plan marks it as per-draw storage, or -1 when it is anything else. */
    private int perDrawStorageSlot(String name) {
        int[] slots = bindings == null ? null : bindings.of(name);
        if (slots == null || slots.length != 1) {
            return -1;
        }

        int slot = slots[0];

        return slot >= 0 && slot < bindings.storageSlots.length && bindings.storageSlots[slot] ? slot : -1;
    }

    /** Draws a collected run: as one batch when it is worth one, and one draw at a time when it is not. */
    private <T> void replayRun(
            List<RenderPass.Draw<T>> run, GpuBuffer indexBuffer, VertexFormat.IndexType indexType) {
        // One draw is not a batch: the indirect call it would take is more work than the draw itself.
        int drawn = run.size() > 1 ? batch(run, indexBuffer, indexType) : 0;

        for (int index = drawn; index < run.size(); index++) {
            replay(run.get(index), indexBuffer, indexType, runInstances[index]);
        }
    }

    /**
     * Records `run` as indirect records and issues them as one `multi_draw_indexed_indirect` per chunk,
     * answering how many of the run's draws were drawn.
     *
     * <p>The run has already been checked to be one run - one pipeline, one vertex buffer, one index
     * buffer, one set of bindings, all of them carrying or not carrying a per-draw record - so this
     * sets them once, and that is the whole saving: what used to be a call across the ABI per draw,
     * with the bind group key and the offsets worked out per draw, is now one call per run.
     *
     * <p>A short answer is not an error. The native side refuses a batch it cannot issue as one call -
     * a device whose multi-draw is emulated, a backend whose implementation of it is broken, a
     * submission that has run out of record space - and the caller draws the rest of the run one draw
     * at a time. The picture is the same either way, which is why this is allowed to be opportunistic.
     */
    private <T> int batch(
            List<RenderPass.Draw<T>> run, GpuBuffer indexBuffer, VertexFormat.IndexType indexType) {
        RenderPass.Draw<T> first = run.get(0);
        setVertexBuffer(first.slot(), first.vertexBuffer());
        setIndexBuffer(indexBuffer, indexType);

        // **The template the native side records the whole run with.** Its parameters are the first
        // draw's and are not what the batch draws with - the records carry those - but the run's state
        // has to be in it, the combination above all. See [writeTemplate].
        writeTemplate(first.firstIndex(), first.indexCount(), first.baseVertex(), 1, true, 0);

        int drawn = 0;

        while (drawn < run.size()) {
            int count = Math.min(BATCH_CHUNK, run.size() - drawn);

            try (Arena chunk = Arena.ofConfined()) {
                MemorySegment records = chunk.allocate(WmNative.BATCH_RECORD, count);

                for (int index = 0; index < count; index++) {
                    RenderPass.Draw<T> draw = run.get(drawn + index);
                    long base = WmNative.elementOffset(WmNative.BATCH_RECORD, index);
                    records.set(WmNative.INT, base + WmNative.BATCH_RECORD_FIRST, draw.firstIndex());
                    records.set(WmNative.INT, base + WmNative.BATCH_RECORD_COUNT, draw.indexCount());
                    records.set(WmNative.INT, base + WmNative.BATCH_RECORD_BASE_VERTEX, draw.baseVertex());
                    records.set(
                            WmNative.INT,
                            base + WmNative.BATCH_RECORD_INSTANCE,
                            Math.max(runInstances[drawn + index], 0));
                }

                MemorySegment array = chunk.allocate(WmNative.RAW_ARRAY);
                WmNative.writeRawArray(array, 0L, records, count);

                int answer = (int) invoke(
                        WmNative.drawCallBatch, device.renderer(), nativePass, drawCall, array);

                if (answer < count) {
                    return drawn + Math.max(answer, 0);
                }
            }

            drawn += count;
        }

        return drawn;
    }

    /**
     * Draws one draw from the state {@link #captureUniformUploads} already put in place.
     *
     * <p>Its own upload has run, so this is the draw itself: the buffers it reads and the record number
     * its vertex stage is to read its per-draw block at. That number goes into the draw call rather
     * than into the call's arguments because Blaze3D's `drawIndexed` has no such parameter - see
     * [WmNative.DRAW_CALL_FIRST_INSTANCE] - and it has to be the same number a batched call would have
     * written into an indirect record, which is what makes the two paths draw the same picture.
     */
    private <T> void replay(
            RenderPass.Draw<T> draw,
            GpuBuffer indexBuffer,
            VertexFormat.IndexType indexType,
            int instance) {
        setVertexBuffer(draw.slot(), draw.vertexBuffer());
        setIndexBuffer(indexBuffer, indexType);

        firstInstance = instance == NO_INSTANCE ? 0 : instance;
        drawIndexed(draw.baseVertex(), draw.firstIndex(), draw.indexCount(), 1);
        firstInstance = 0;
    }

    /**
     * The first instance of the draw being recorded.
     *
     * <p>Blaze3D's draw calls have no such parameter, so it travels beside them: [replay] sets it for
     * one draw and clears it, and [record] writes it into the call. See
     * [WmNative.DRAW_CALL_FIRST_INSTANCE].
     */
    private int firstInstance = 0;

    /**
     * How many draws one batched call carries at most.
     *
     * <p>The array of records is written into a confined arena per call, so this is what bounds that
     * allocation: 4096 records is 64 KiB, and a run longer than this becomes several calls - which is
     * still a handful of calls per run rather than one per draw. The native side has a record buffer of
     * its own and refuses what does not fit in it, so this number only has to be reasonable, not exact.
     */
    private static final int BATCH_CHUNK = 4096;

    /**
     * Diagnostics: what the multi-draw calls carry, and how many draws a batched path would issue.
     *
     * <p>Minecraft hands the whole list of a layer's sections to one call of {@link
     * #drawMultipleIndexed}, and the renderer replays it one draw at a time. This counts both
     * numbers, because they are the whole case for batching: "draws" is what the game asked for, and
     * "runs" is what {@code multi_draw_indexed_indirect} would issue - one call per run of
     * consecutive draws that share a vertex buffer, an index buffer and an index format, which is the
     * one thing an indirect record cannot change per draw.
     *
     * <p>A batch is only worth issuing if the draws in it also agree about everything else the draw
     * carries - the pipeline, the bind groups, the offsets that travel with a draw - so the run count
     * is the optimistic number: it is what a batched path would issue if every draw of the run were
     * batchable, and it is the number to compare against the calls the frame actually made.
     */
    private <T> void countMultiDraw(
            Collection<RenderPass.Draw<T>> draws,
            GpuBuffer defaultIndexBuffer,
            VertexFormat.IndexType defaultIndexType) {
        int runs = 0;
        GpuBuffer vertex = null;
        GpuBuffer index = null;
        VertexFormat.IndexType type = null;

        for (RenderPass.Draw<T> draw : draws) {
            GpuBuffer drawIndex = draw.indexBuffer() != null ? draw.indexBuffer() : defaultIndexBuffer;
            VertexFormat.IndexType drawType =
                    draw.indexType() != null ? draw.indexType() : defaultIndexType;

            if (vertex != draw.vertexBuffer() || index != drawIndex || type != drawType) {
                runs++;
                vertex = draw.vertexBuffer();
                index = drawIndex;
                type = drawType;
            }
        }

        String pipeline = activePipelineName == null ? "<no pipeline>" : activePipelineName;
        long[] totals = MULTI_DRAW.computeIfAbsent(pipeline, key -> new long[3]);

        synchronized (totals) {
            totals[0]++;
            totals[1] += draws.size();
            totals[2] += runs;
        }

        if (multiDrawReportDue()) {
            reportMultiDraw();
        }
    }

    /** One pipeline's `{calls, draws, runs}` since the last report. */
    private static final Map<String, long[]> MULTI_DRAW = new java.util.concurrent.ConcurrentHashMap<>();

    /** Frames presented when the last report was made, so the numbers can be given per frame. */
    private static long multiDrawFrames = 0L;

    private static long lastMultiDrawReport = 0L;

    /** Whether a report is due, at most once a second. See [reportCloudDraw] for the same shape. */
    private static boolean multiDrawReportDue() {
        long now = System.nanoTime();
        if (now - lastMultiDrawReport < 1_000_000_000L) {
            return false;
        }

        lastMultiDrawReport = now;
        return true;
    }

    /** Logs the counts of every pipeline that was drawn through a multi-draw call, then clears them. */
    private static void reportMultiDraw() {
        long frames = WgpuDevice.getFramesPresented();
        long window = frames - multiDrawFrames;
        multiDrawFrames = frames;

        StringBuilder line = new StringBuilder();

        for (Map.Entry<String, long[]> entry : MULTI_DRAW.entrySet()) {
            long calls;
            long draws;
            long runs;

            synchronized (entry.getValue()) {
                calls = entry.getValue()[0];
                draws = entry.getValue()[1];
                runs = entry.getValue()[2];
                entry.getValue()[0] = 0L;
                entry.getValue()[1] = 0L;
                entry.getValue()[2] = 0L;
            }

            if (draws == 0L) {
                continue;
            }

            if (line.length() > 0) {
                line.append(", ");
            }

            line.append(entry.getKey())
                    .append(": ")
                    .append(draws)
                    .append(" draw(s) in ")
                    .append(calls)
                    .append(" call(s), ")
                    .append(runs)
                    .append(" run(s)");
        }

        if (line.length() == 0) {
            return;
        }

        dev.birb.wgpu.WgpuMcMod.LOGGER.info(
                "wgpu: multi-draw over {} frame(s): {}", Math.max(window, 1L), line);
    }

    @Override
    public boolean isClosed() {
        return closed.get();
    }

    @Override
    public void close() {
        if (closed.compareAndSet(false, true)) {
            // A pass the graph took over has no native side left to drop: it was finished when the
            // takeover happened, because that is the only order wgpu allows.
            if (!nativePass.equals(MemorySegment.NULL)) {
                invoke(WmNative.dropRenderPass, nativePass);
            }

            // The bind groups went with the pass itself and the draw call buffer goes back to the
            // pool for the next pass on this thread: nothing here frees anything per draw.
            drawCallBuffer.release();
            encoder.onRenderPassClosed();
            // After the pass has closed, so the target holds a render result rather than whatever it
            // held before it ran - and submitted for, because this dump is a readback and the pass's
            // own commands are still waiting in the frame's encoder. Only for a pass that is dumped:
            // submitting for every pass of every frame is what this backend just stopped doing.
            // The dump switch, not the log switch: this writes files.
            boolean dumpPass = Diagnostics.dumpsEnabled() && Diagnostics.dumpsPass(label);
            boolean dumpItemAtlas = drawsIntoTheItemAtlas() && Diagnostics.itemAtlasDumpDue();

            if (dumpPass || dumpItemAtlas) {
                encoder.submitForReadback();

                if (dumpPass) {
                    Diagnostics.dumpPassTarget(device.renderer(), label, nativeColorTexture);
                }
                if (dumpItemAtlas) {
                    Diagnostics.dumpTexture(
                            device.renderer(), nativeColorTexture, "pass-ui-items-atlas.raw", true);
                }
            }
        }
    }
}



