package dev.birb.wgpu

import dev.birb.wgpu.rust.RendererSettings
import net.minecraft.client.Minecraft
import net.neoforged.bus.api.SubscribeEvent
import net.neoforged.fml.common.EventBusSubscriber
import net.neoforged.neoforge.client.event.ClientTickEvent

/**
 * Holds the camera still while the renderer's `pin_camera` setting is on.
 *
 * **Every frame comparison this project attempted was invalid without this.** Two runs of the same
 * world, from the same save, do not stand in the same place: the player is still falling, still
 * sliding, still being nudged by whatever the spawn put them next to, and a frame number does not mean
 * the same view if the view drifted. Two past attempts at an A/B produced 45-50% "differences" that were
 * nothing but two different camera positions, and one pair of dumps turned out to be the same file
 * copied twice.
 *
 * It freezes the player where the world put them and zeroes the motion every tick, because a pinned
 * *starting point* is not a pinned camera: gravity is what moved the two runs apart, and zeroing the
 * velocity is what makes the position a constant rather than a beginning. The previous positions are
 * copied across too, so the render thread's interpolation between ticks has nothing to interpolate.
 *
 * **A setting and not a marker file**, like every other switch in this renderer: `pin_camera` is in the
 * schema, on the options screen under Debug, and persisted in `config/wgpu-mc-renderer.json`. Reading it
 * is the cached reader (`RendererSettings`, one refresh a second), so a tick costs a hash lookup.
 *
 * Client ticks rather than frames, like [DebugReload]: a tick is where a position can be held, and the
 * render thread reads whatever the last tick left.
 */
@EventBusSubscriber(modid = WgpuMcMod.MOD_ID)
object DebugCamera {
	private const val PIN_CAMERA = "pin_camera"

	private var reported = false

	@JvmStatic
	@SubscribeEvent
	fun onClientTick(event: ClientTickEvent.Post) {
		if (RendererSettings.bool(PIN_CAMERA) != true) {
			return
		}

		val player = Minecraft.getInstance().player ?: return

		player.setDeltaMovement(0.0, 0.0, 0.0)
		player.fallDistance = 0.0
		player.yRotO = player.yRot
		player.xRotO = player.xRot
		player.xo = player.x
		player.yo = player.y
		player.zo = player.z

		if (!reported) {
			reported = true

			WgpuMcMod.LOGGER.info(
				"wgpu: `{}` is on, so the camera is pinned at {} {} {} (yaw {}, pitch {})",
				PIN_CAMERA,
				player.x,
				player.y,
				player.z,
				player.yRot,
				player.xRot,
			)
		}
	}
}