package dev.birb.wgpu.render

import net.minecraft.client.Minecraft
import org.joml.Matrix4f
import java.nio.file.Files
import java.nio.file.Path

/**
 * 每帧的亚像素抖动偏移，TAA/TAAU 的采样序列。
 *
 * 时序抗锯齿靠的是"同一像素在不同帧采到不同子像素位置"：把投影矩阵在裁剪空间里平移不到一个像素的
 * 量，画面就会在每帧落在略微不同的采样格点上，history 再把这些样本累积起来。
 *
 * **必须同时作用于地形和 Minecraft 自己的绘制。** 地形走 [TerrainPass.sendCameraMatrices] 合成的那份
 * 投影，MC 自己的绘制走 `ProjectionMatrixBuffer` 里的那份 UBO —— 见
 * `dev.birb.wgpu.mixin.render.ProjectionMatrixBufferMixin`。两处只要有一个没抖，TAA 就会把偏移差当成
 * 运动，表现为撕裂或拖影。所以两边都调用这里的同一个 [apply]，用同一个像素偏移。
 *
 * 开关按这个后端既有的约定解析：系统属性 → 环境变量 → 运行目录下的标记文件。默认关，因为单有抖动、
 * 没有 TAA resolve 时画面只会"晃"——那是阶段 2 的验证信号，不是可交付状态。
 */
object TaaJitter {
    /** 采样序列的长度。16 个点足以铺满一个像素而不产生可见的低频图样。 */
    private const val SAMPLES = 16

    /** 亚像素偏移，单位是**像素**，范围 [-0.5, 0.5]。 */
    @Volatile private var pixelX = 0.0f
    @Volatile private var pixelY = 0.0f

    private var sample = 0

    /**
     * 哪些投影要抖。名字就是 `ProjectionMatrixBuffer` 构造时的 name。
     *
     * `level`（关卡）与 `3d hud`（第一人称手持物，和关卡同一空间）必须抖；`gui`/`post`/`items`/
     * `PIP - ...` 绝不能抖——GUI 被时序化就是文字发糊。`cubemap`（天空盒）暂时不抖：它没有可用的速度
     * 向量，抖了只会在 TAA 里被当成运动，等阶段 6 连速度一起处理。
     */
    private val WORLD_PROJECTIONS = setOf("level", "3d hud")

    @Volatile private var enabled: Boolean = resolveEnabled()

    private fun resolveEnabled(): Boolean {
        System.getProperty("wgpu_mc.taa")?.toBoolean()?.let { return it }
        System.getenv("WGPU_MC_TAA")?.let { return it != "0" && !it.equals("false", ignoreCase = true) }
        return Files.exists(Path.of("wgpu-taa"))
    }

    /** 抖动是否开着。进程启动时解析一次，改开关要重启。 */
    @JvmStatic
    fun isEnabled(): Boolean = enabled

    /** [name] 对应的投影是否要施加抖动。 */
    @JvmStatic
    fun jittersProjection(name: String?): Boolean =
        enabled && name != null && WORLD_PROJECTIONS.contains(name)

    /** 本帧的亚像素偏移，单位像素。TAA resolve 用它把 history 反投影回本帧的采样格点。 */
    @JvmStatic fun offsetPixelsX(): Float = pixelX
    @JvmStatic fun offsetPixelsY(): Float = pixelY

    /**
     * 推进到本帧的采样点。每帧一次，从 `GameRendererMixin` 的 `render` HEAD 调用——那是这个 mod 已有的
     * "每帧一次"的位置，而且早于本帧任何投影被写入。
     */
    @JvmStatic
    fun advanceFrame() {
        if (!enabled) {
            pixelX = 0.0f
            pixelY = 0.0f
            return
        }

        sample = (sample + 1) % SAMPLES
        val index = sample + 1
        pixelX = halton(index, 2) - 0.5f
        pixelY = halton(index, 3) - 0.5f
    }

    /**
     * 把本帧的像素偏移施加到 [matrix] 上，原地修改并返回同一个矩阵。
     *
     * 裁剪空间里给第 0 行加上第 3 行的 k 倍，就等于把 NDC 的 x 平移 k（因为透视除法会约掉 w）。JOML 的
     * 访问器命名是 `m<列><行>`——`TerrainPass` 里读 bob 平移用的 `m30` 就是列 3、行 0，可作旁证——而透视
     * 矩阵的第 3 行是 `(0, 0, -1, 0)`，所以这一项落在第 2 列上，改的是 `m20`/`m21`。
     *
     * 符号只决定采样图样的走向，不决定正确性：关键是**两个调用点用同一个函数**，这样地形与 MC 自己的
     * 绘制一定落在同一个子像素位置上。
     */
    @JvmStatic
    fun apply(matrix: Matrix4f, width: Int, height: Int): Matrix4f {
        if (!enabled || width <= 0 || height <= 0) return matrix

        matrix.m20(matrix.m20() - 2.0f * pixelX / width.toFloat())
        matrix.m21(matrix.m21() - 2.0f * pixelY / height.toFloat())
        return matrix
    }

    /** 本帧渲染目标的宽度（像素），抖动的分母。 */
    @JvmStatic
    fun renderWidth(): Int = Minecraft.getInstance()?.window?.width ?: 0

    /** 本帧渲染目标的高度（像素）。见 [renderWidth]。 */
    @JvmStatic
    fun renderHeight(): Int = Minecraft.getInstance()?.window?.height ?: 0

    /** [index] 在 [base] 进制下的 Halton 值，落在 [0, 1)。 */
    private fun halton(index: Int, base: Int): Float {
        var f = 1.0f
        var r = 0.0f
        var i = index
        while (i > 0) {
            f /= base.toFloat()
            r += f * (i % base)
            i /= base
        }
        return r
    }
}
