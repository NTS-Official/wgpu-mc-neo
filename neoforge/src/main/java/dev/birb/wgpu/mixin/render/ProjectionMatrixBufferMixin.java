package dev.birb.wgpu.mixin.render;

import com.mojang.blaze3d.buffers.GpuBufferSlice;
import com.mojang.blaze3d.systems.RenderSystem;
import dev.birb.wgpu.render.TaaJitter;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import net.minecraft.client.renderer.Projection;
import net.minecraft.client.renderer.ProjectionMatrixBuffer;
import org.joml.Matrix4f;
import org.spongepowered.asm.mixin.Mixin;
import org.spongepowered.asm.mixin.Unique;
import org.spongepowered.asm.mixin.injection.At;
import org.spongepowered.asm.mixin.injection.Inject;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfo;
import org.spongepowered.asm.mixin.injection.callback.CallbackInfoReturnable;

/**
 * 把每帧的亚像素抖动写进 Minecraft 自己的投影 UBO —— TAA/TAAU 的第二个注入点。
 *
 * <p>第一个在 {@code TerrainPass.sendCameraMatrices}（Rust 地形那条路自己合成投影）。这一个覆盖
 * Minecraft 自己的每一次绘制：实体、方块实体、粒子、天空、第一人称手持物。两边必须用同一个像素偏移，
 * 否则 TAA 会把偏移差读成运动，表现为撕裂或拖影。
 *
 * <p>为什么要挂在 {@code ProjectionMatrixBuffer} 上：26.1 把投影 UBO 收进了这一个类，每个用途一个实例、
 * 构造时带一个名字（{@code "level"}、{@code "3d hud"}、{@code "gui"}、{@code "post"}、{@code "cubemap"}、
 * {@code "items"}、{@code "PIP - ..."}），而名字正好就是"该不该抖"的分界。详见 {@code TaaJitter}。
 *
 * <p>为什么注入在 {@code getBuffer} 的 RETURN 而不是在 {@code writeBuffer} 里：{@code getBuffer} 在投影
 * 与其版本号都没变时**直接返回、不重写**（{@code ProjectionMatrixBuffer:34-35}），相机静止时那一帧就
 * 不会写入新抖动，TAA 也就拿不到新的子像素样本。挂在 RETURN 上是"每帧写一次"，而 {@code getBuffer}
 * 每个实例每帧恰好被调用一次（{@code GameRenderer:721}、{@code :747}）。
 *
 * <p>返回值就是那块 slice（{@code writeBuffer} 返回的也是同一个 {@code bufferSlice}），所以既不需要
 * {@code @Accessor}，也不需要 {@code @Invoker}。
 */
@Mixin(ProjectionMatrixBuffer.class)
public abstract class ProjectionMatrixBufferMixin {

    /** 这个实例构造时的名字，决定它是不是一份"世界投影"。 */
    @Unique
    private String wgpuMc$name = "";

    /** 抖动前那份投影的落脚点，也是写进 UBO 的那 64 字节的来源。 */
    @Unique
    private final Matrix4f wgpuMc$base = new Matrix4f();

    /** 复用同一块堆外内存：每帧每份世界投影一次，不值得每次分配。 */
    @Unique
    private final ByteBuffer wgpuMc$bytes =
            ByteBuffer.allocateDirect(RenderSystem.PROJECTION_MATRIX_UBO_SIZE).order(ByteOrder.nativeOrder());

    @Inject(method = "<init>", at = @At("TAIL"))
    private void wgpuMc$rememberName(String name, CallbackInfo ci) {
        this.wgpuMc$name = name;
    }

    @Inject(
            method = "getBuffer(Lnet/minecraft/client/renderer/Projection;)Lcom/mojang/blaze3d/buffers/GpuBufferSlice;",
            at = @At("RETURN"))
    private void wgpuMc$jitterTheProjection(Projection projection, CallbackInfoReturnable<GpuBufferSlice> cir) {
        if (!TaaJitter.jittersProjection(this.wgpuMc$name)) {
            return;
        }

        GpuBufferSlice slice = cir.getReturnValue();
        if (slice == null) {
            return;
        }

        // The *unjittered* matrix is the input, every frame: jitter is a fresh sub-pixel offset, not an
        // accumulated one, so applying it to last frame's result would walk the projection away.
        projection.getMatrix(this.wgpuMc$base);
        TaaJitter.apply(this.wgpuMc$base, TaaJitter.renderWidth(), TaaJitter.renderHeight());

        this.wgpuMc$bytes.clear();
        this.wgpuMc$base.get(this.wgpuMc$bytes);
        this.wgpuMc$bytes.rewind();

        // The same call Minecraft itself makes in `ProjectionMatrixBuffer.writeBuffer`, on the same
        // shared encoder - so this is one more 64 byte write in a frame that already has this one, and
        // it lands before the frame's draws because `getBuffer` is called before them.
        RenderSystem.getDevice().createCommandEncoder().writeToBuffer(slice, this.wgpuMc$bytes);
    }
}
