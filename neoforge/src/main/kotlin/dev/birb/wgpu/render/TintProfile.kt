package dev.birb.wgpu.render

import net.minecraft.client.Minecraft
import net.minecraft.client.renderer.BiomeColors
import net.minecraft.core.BlockPos
import net.minecraft.world.level.block.Blocks
import net.minecraft.world.level.material.Fluids

/**
 * A row of biome tints across the nearest biome boundary, once a second.
 *
 * **The question this exists for**: the tint is right inside a biome and wrong where two meet. Reading the
 * code says it should not be - our `Wgpu.helperGetBlockColor` / `helperGetSectionTints` ask the game's own
 * `BlockTintSource#colorInWorld`, which is the same call the game's own model baking makes, at the same
 * position, through the same `ClientLevel#getBlockTint` cache and the same `(2 * biomeBlendRadius + 1)^2`
 * average. So either the inputs are not what this side thinks they are, or the tint is fine and the picture
 * problem is somewhere else - and neither of those can be read off the code.
 *
 * What the line shows: one character of biome per block, and the tint's red channel in hex per block, along a
 * row that crosses a biome change. **A blend of radius r starts moving r blocks before the biome letter
 * changes**; a tint that only moves *at* the change is a hard edge, and a tint that moves where the letters
 * do not is being read from somewhere else entirely.
 *
 * The colours come from the same path the bake uses - the tint source of `grass_block`, index 0 - and are
 * compared against `ClientLevel#calculateBlockTint`, which is what the cache computes on a miss. The two
 * disagreeing means the *cache* is holding a value the world no longer produces, which is the one way a
 * correct tint can reach the screen stale.
 *
 * Called from the terrain pass's once-a-second diagnostics, so it costs nothing when the log is off.
 */
object TintProfile {

	/** How far either side of the boundary the row is read, and how far the search for one goes. */
	private const val ROW = 16
	private const val SEARCH = 32

	/** How many distinct biomes the letters can name before they wrap, which is `A`..`Z`. */
	private const val LETTERS = 26

	private val pos = BlockPos.MutableBlockPos()
	private val probe = BlockPos.MutableBlockPos()
	private val search = BlockPos.MutableBlockPos()

	/**
	 * Finds the nearest place two neighbouring blocks disagree about their biome, and reads the row through
	 * it.
	 *
	 * The comparison is `getBiome`, which is the same call the tint makes (`LevelReader#getBiome` is
	 * `BiomeManager#getBiome`, the fiddled three-biome blend), so the letters mark exactly the changes the
	 * tint is supposed to follow. A world where nothing changes within [SEARCH] blocks prints the row from
	 * the camera and says so rather than nothing - "no boundary here" is an answer.
	 */
	fun report() {
		val client = Minecraft.getInstance()
		val level = client.level ?: return

		val camera = client.gameRenderer.mainCamera.blockPosition()
		val y = camera.y

		// The nearest change, in whichever of the two axes it happens on, so the row can be read across it.
		var found: Triple<Int, Int, Boolean>? = null

		for (radius in 1..SEARCH) {
			for (step in -radius..radius) {
				// The ring at this radius, so the first hit is the nearest one.
				val ring = listOf(step to -radius, step to radius, -radius to step, radius to step)

				for ((dx, dz) in ring) {
					if (found != null) {
						break
					}

					val here = level.getBiome(search.set(camera.x + dx, y, camera.z + dz)).unwrapKey()
					val next = level.getBiome(search.set(camera.x + dx + 1, y, camera.z + dz)).unwrapKey()

					if (here != next) {
						found = Triple(camera.x + dx, camera.z + dz, true)
					}
				}
			}

			if (found != null) {
				break
			}
		}

		val (anchorX, anchorZ, alongX) = found ?: Triple(camera.x, camera.z, true)

		val biomes = LinkedHashMap<String, Char>()
		val letters = StringBuilder()
		val red = StringBuilder()
		val blue = StringBuilder()
		var mismatched = 0
		var differing = 0

		for (offset in -ROW..ROW) {
			// Across the change where one was found, and along x from the camera where none was.
			val x = if (alongX) anchorX + offset else anchorX
			val z = if (alongX) anchorZ else anchorZ + offset

			pos.set(x, y, z)

			val key = level.getBiome(pos).unwrapKey()
				.map { it.identifier().toString() }
				.orElse("<unregistered>")
			val letter = biomes.getOrPut(key) {
				('A' + biomes.size % LETTERS)
			}

			letters.append(letter)

			val state = Blocks.GRASS_BLOCK.defaultBlockState()
			val source = client.blockColors.getTintSource(state, 0)
			val tint = source?.colorInWorld(state, level, pos) ?: -1

			// The cache's answer against the arithmetic behind it. They are the same call on a miss; on a hit
			// the second one is what the world looks like now, which is what a section meshed before a
			// neighbouring chunk arrived would have missed.
			val cached = level.getBlockTint(pos, BiomeColors.GRASS_COLOR_RESOLVER)
			val direct = level.calculateBlockTint(pos, BiomeColors.GRASS_COLOR_RESOLVER)

			if (cached != direct) {
				mismatched++
			}

			if (tint != cached) {
				differing++
			}

			val rr = (tint shr 16) and 0xff

			red.append("%02x".format(rr))

			// **The *fluid* tint, which is a different source from the block tint.** `Blocks.WATER`'s own
			// registered block tint is `waterParticles()`, whose `colorInWorld` is white - the water a player
			// sees is tinted by the *fluid* model's source. Its blue channel is the one printed: water's
			// colour across biomes runs from `3f76e4` to `617b64`, which is 128 apart in blue and 20 in red,
			// so a row that ramps in red and not in blue (or neither) says which of the two paths moved.
			val water = Fluids.WATER.defaultFluidState()
			val waterSource = client.modelManager.fluidStateModelSet.get(water).fluidTintSource()
			val waterTint = waterSource?.colorInWorld(water, state, level, pos) ?: -1

			blue.append("%02x".format(waterTint and 0xff))
		}

		val legend = biomes.entries.joinToString(" ") { "${it.value}=${it.key}" }

		dev.birb.wgpu.WgpuMcMod.LOGGER.info(
			"wgpu: tint row {} ({} block(s), {}), biome [{}], grass red [{}], water blue [{}]; {} block(s) whose cached colour is not what the world now says, {} not the tint source's answer",
			if (alongX) "across x at z=$anchorZ" else "across z at x=$anchorX",
			ROW * 2 + 1,
			if (found != null) "a biome change" else "no change within $SEARCH block(s) - read from the camera",
			letters,
			red,
			blue,
			mismatched,
			differing,
		)

		dev.birb.wgpu.WgpuMcMod.LOGGER.info("wgpu: tint row biomes: {}", legend)
	}
}