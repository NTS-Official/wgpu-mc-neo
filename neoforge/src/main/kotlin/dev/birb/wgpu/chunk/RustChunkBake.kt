package dev.birb.wgpu.chunk

import dev.birb.wgpu.WgpuMcMod
import dev.birb.wgpu.backend.Diagnostics
import dev.birb.wgpu.mixin.chunk.RenderSectionRegionAccessor
import dev.birb.wgpu.mixin.world.PackedIntegerArrayMixin
import dev.birb.wgpu.mixin.chunk.SectionCopyAccessor
import dev.birb.wgpu.palette.RustBlockStateAccessor
import net.minecraft.world.level.material.Fluids

import dev.birb.wgpu.rust.RendererSettings
import dev.birb.wgpu.rust.WgpuNative
import dev.birb.wgpu.rust.WmNative
import net.minecraft.client.Minecraft
import net.minecraft.client.renderer.chunk.RenderSectionRegion
import net.minecraft.core.SectionPos
import net.minecraft.util.SimpleBitStorage
import net.minecraft.world.level.LightLayer
import net.minecraft.world.level.block.state.BlockState
import net.minecraft.world.level.chunk.PalettedContainer
import java.lang.foreign.Arena
import java.lang.foreign.MemorySegment
import java.lang.foreign.ValueLayout
import java.nio.file.Files
import java.nio.file.Path
import java.util.concurrent.ConcurrentHashMap
import kotlin.math.min

/**
 * Hands a section rebuild to the Rust terrain baker.
 *
 * The Rust side owns the meshing (`wgpu_mc::mc::chunk::bake_layers`) and the arena the result lands
 * in (`SectionStorage`); this side owns the world. What crosses the boundary is Minecraft's own data,
 * not a copy shaped for Rust:
 *
 *  - **The block storage goes over as it is.** A `PalettedContainer` is a palette plus a bit-packed
 *    storage, and the storage's geometry - `valuesPerLong`, `mask`, `divideMul`, `divideAdd`,
 *    `divideShift` and the raw longs - is read straight out of Minecraft's `SimpleBitStorage`. What
 *    Rust gets beside it is a **palette translation table**: `table[minecraft index]` is the block key
 *    Rust knows that state by. Two entries may name the same key - a waterlogged variant, say - which
 *    is exactly why the table exists: the indices stay Minecraft's, so nothing has to be renumbered
 *    and no storage has to be re-packed. (An earlier revision rebuilt a palette and packed a fresh
 *    `SimpleBitStorage` here, per section, per rebuild: 4096 `getAll` calls and a new bit-packing
 *    every time, for data the game was already holding.)
 *  - **Only what changed is sent.** Rust keeps a section until it is far from the player, and this side
 *    keeps what it has told Rust, so a section whose storage and light hash the same as last time goes
 *    over as one bit in a mask instead of a few kilobytes. Light especially: a rebuild of one section
 *    needs the light of 27, and re-sending all of them was 108 KB of copying, 54 array allocations and
 *    54 JNI calls each time, for data that changes when a torch is placed rather than when a chunk is
 *    re-meshed. What that bookkeeping has to get right is the difference between "nothing to say" and
 *    "there is nothing there": a section with no blocks is sent as *absent* every time rather than
 *    left out, and [sent] records that as an entry with a null in it - a missing entry would make the
 *    next rebuild compare `null` against nothing and find nothing to say, and the copy Rust kept would
 *    be meshed for the rest of the session;
 *  - **Everything goes in one call.** The payload is written into one reusable off-heap buffer and
 *    handed over as an address; the old shape was ~57 JNI calls and 60 arrays per rebuild, and it
 *    allocated all of them per rebuild as well.
 *
 * The two sides also have to agree on *which world* they are describing, which is what [generation]
 * is for: a rebuild that was already running when the player changed dimension would otherwise write
 * the old world's blocks under the new world's coordinates, and be recorded as sent besides. Every
 * record carries the number, the native side refuses the ones that are not current - it answers with
 * a bit per refused slot - and only the accepted ones are recorded. A refusal costs one re-send, which
 * is the next rebuild of that section; the two ends of a level change are `forgetAll` here and
 * `clearSections` there, both called from `LevelRenderer#setLevel`.
 *
 * A third way the two sides can disagree is the arena: a section it has no room for is one this side
 * has been told was baked and which is not drawn at all, and its rebuild has already happened. The
 * arena keeps the *positions* it refused, the JVM drains them once a tick and forgets what it had
 * recorded for each - one re-send per section rather than a hole that stays for the session.
 *
 * Three things about the shape of the data are worth naming, because each is easy to get wrong:
 *
 *  - **The 27 entries are ordered x fastest, then y, then z.** That is the order the Rust provider
 *    indexes (`sx + sy * 3 + sz * 9`) and the order `RenderSectionRegion` keeps its own copies in.
 *  - **Light is a nibble array**, 2048 bytes for a 16x16x16 section, packed as
 *    `(y << 8) | (z << 4) | x` with the low nibble first - `DataLayer#getData` is exactly that.
 *  - **The sections come from the rebuild's own snapshot.** A `SectionCopy` holds a private copy of
 *    the section's `PalettedContainer`, so reading it is both consistent with the mesh Minecraft is
 *    building in the same task and safe off-thread (the live chunk is guarded by a threading
 *    detector). A position the snapshot does not cover is "not loaded", which is air to both sides.
 *
 * Nothing here runs unless the path is switched on - see [SETTING] - and it is one switch for both
 * halves: the sections baked here are the ones the render graph's terrain pass draws in place of
 * Minecraft's own opaque layer, so turning it off is what puts that layer back.
 */
object RustChunkBake {
	/**
	 * The setting the path is switched on by: `terrain`, under the renderer's own settings.
	 *
	 * One switch for the whole `@geo_terrain` path, which is two halves that only make sense together:
	 * this side meshes the sections, and the render graph's terrain pass draws them *instead of*
	 * Minecraft's own opaque layer (see `TerrainPass`). Off, both halves are off and the layer is
	 * Minecraft's again.
	 *
	 * It was a marker file (`wgpu-geo-terrain`) while the path was being brought up, which is a file
	 * name a player has to know and a restart to change. A setting is the right shape for it: the
	 * options screen reaches it, and flipping it *on* rebuilds the sections, because the graph can only
	 * draw what the baker has baked.
	 */
	private const val SETTING = "terrain"

	/** How often [isOn] may go and ask the settings for the switch. */
	private const val SETTING_POLL_NANOS = 1_000_000_000L

	private const val SECTIONS = 27
	private const val AXIS = 3
	private const val LIGHT_BYTES = 2048
	private const val REPORT_EVERY = 64L

	/**
	 * How many sections the "what have I already sent" table may hold before it is dropped.
	 *
	 * Losing it costs payloads with data in them that Rust already has - the masks stop claiming
	 * anything - so the cap only has to keep the table from growing with the world.
	 */
	private const val SENT_LIMIT = 32768

	/** The header, the 27 block records and the 27 light records; the blobs follow. */
	private const val HEADER_BYTES = 32L
	private const val BLOCK_RECORD_BYTES = 64L
	private const val LIGHT_RECORD_BYTES = 16L
	private const val BLOBS_AT = HEADER_BYTES + SECTIONS * BLOCK_RECORD_BYTES + SECTIONS * LIGHT_RECORD_BYTES

	/**
	 * Where a record says which world it describes: the last word of each record shape, which both
	 * had spare. See [RustChunkBake.forgetAll] and the `SECTION_GENERATION` docs in
	 * `rust/wgpu-mc-jni/src/section.rs`.
	 */
	private const val BLOCK_RECORD_GENERATION_WORD = 15
	private const val LIGHT_RECORD_GENERATION_WORD = 3

	/** Must match `PAYLOAD_MAGIC` in `rust/wgpu-mc-jni/src/section.rs`. */
	private const val MAGIC = 0x574D5332

	/** Must match `REJECTED_MASK` in `rust/wgpu-mc-jni/src/section.rs`: which slots were refused. */
	private const val REJECTED_MASK = 0x07FF_FFFF

	/** Must match `RESYNC` in `rust/wgpu-mc-jni/src/section.rs`: send the neighbourhood again. */
	private const val RESYNC = 1 shl 27

	/**
	 * One buffer per build thread, big enough for the worst case: 27 sections of a 32-bit-per-entry
	 * storage is 16 KB each, plus their palettes, plus 27 sections of light. It is allocated once and
	 * never grows, so a rebuild allocates nothing at all.
	 */
	private const val PAYLOAD_BYTES = 1024L * 1024L

	private val EMPTY_LIGHT = ByteArray(LIGHT_BYTES)

	/** Whether the path is on, as last read from the settings. Read per rebuild and per frame. */
	@Volatile
	private var enabled = true

	/** When the switch was last read from the settings, so the read is not per rebuild. */
	@Volatile
	private var enabledAt = 0L

	/** Whether the state has been said in the log yet, so the first read always reports it. */
	@Volatile
	private var reportedState = false

	/** Whether the Rust terrain baker runs at all. */
	@JvmStatic
	fun isOn(): Boolean {
		val now = System.nanoTime()

		// Once a second, and on the first ask. `isOn` is called for every section Minecraft meshes and
		// for every frame the terrain pass is taken over for, and reading the setting is a JNI call and
		// a JSON parse - see `RendererSettings` - so the value is kept and this is what moves it.
		if (enabledAt == 0L || now - enabledAt >= SETTING_POLL_NANOS) {
			enabledAt = now
			refresh()
		}

		return enabled
	}

	/**
	 * Reads the switch back out of the renderer's settings and applies it.
	 *
	 * Switching it *on* has to reach the sections that are already meshed: the graph draws the arena's
	 * contents, and nothing rebuilds a section whose blocks have not changed. Asking the level renderer
	 * for all of them is the same call the feed makes when the block registry arrives.
	 */
	private fun refresh() {
		val previous = enabled
		enabled = RendererSettings.bool(SETTING) ?: true

		// Once a second, and cheap: the return channel's own arithmetic, which is what tells a broken
		// bridge apart from a full arena. See `checkRefusalChannel`.
		checkRefusalChannel()

		if (enabled == previous && reportedState) {
			return
		}

		reportedState = true

		WgpuMcMod.LOGGER.info(
			"wgpu: the Rust terrain path is {} (the renderer's `{}` setting)",
			if (enabled) "on" else "off",
			SETTING,
		)

		if (enabled && enabled != previous) {
			Minecraft.getInstance().execute { Minecraft.getInstance().levelRenderer.allChanged() }
		}
	}

	private var bakes = 0L
	private var reported = 0L
	private var resyncs = 0L

	/** How many refused sections this side has drained. See [checkRefusalChannel]. */
	private var refusedDrained = 0L

	/** The mismatch the channel check last saw, and whether it has been reported. See [checkRefusalChannel]. */
	private var refusedMissing = 0L
	private var refusedUnaccounted = 0L
	private var refusedReported = false

	/**
	 * Diagnostics: this side's half of the return channel, for the line the terrain pass logs - the
	 * native side's half is the refusal count already on it. See [checkRefusalChannel] for the
	 * arithmetic the two have to satisfy.
	 */
	@JvmStatic
	val refusedDiagnostics: String
		get() = "$refusedDrained refused section(s) drained"

	/**
	 * Forgets the sections the arena had no room for, by the keys the native side hands over.
	 *
	 * A refusal is a section this side has been told was baked and which never reached the arena, so
	 * nothing draws it - and because its rebuild has already happened, and a rebuild carries only what
	 * changed, nothing would offer it again either: a hole that stays for the session, and silent.
	 * Dropping its entry from [sent] is what makes the next rebuild of that section carry its blocks
	 * again, so it gets another chance at the pool; and a section that is refused again is handed over
	 * again, so the two sides keep converging rather than giving up after one try.
	 *
	 * Called once a client tick - often enough that a refusal is undone before the player can see it,
	 * and it is one native call that returns an empty array in the normal case. The keys are
	 * `SectionPos.asLong`, which is the key [sent] already uses, so nothing is unpacked.
	 */
	@JvmStatic
	fun forgetRefused() {
		val refused = try {
			WgpuNative.refusedSections()
		} catch (error: Throwable) {
			// No renderer yet, so nothing has been baked and nothing can have been refused.
			return
		}

		if (refused.isEmpty()) {
			return
		}

		for (key in refused) {
			sent.remove(key)
		}

		refusedDrained += refused.size

		WgpuMcMod.LOGGER.warn(
			"wgpu: the section arena refused {} section(s); forgetting them, so the next rebuild " +
				"of each carries its blocks again ({} drained so far)",
			refused.size,
			refusedDrained,
		)
	}

	/**
	 * Checks the arena's return channel against this side's own count.
	 *
	 * The native side counts three things - the refusals, how many of them it handed over, and how
	 * many it lost before it could (past its list's cap, or cleared by a level change) - and they add
	 * up: `refused = handed over + dropped`. This side counts the fourth: what it actually drained.
	 * So the check is two subtractions, and either one being short is a broken channel rather than a
	 * full arena - a refusal that was neither delivered nor accounted for is a section this side still
	 * believes was baked, and nothing will offer it again.
	 *
	 * Run with the settings poll, once a second, and it reports a mismatch only once it has been seen
	 * twice in a row. That is not caution: a drain happening on the render thread right now shows up
	 * here as one refusal the native side has counted and this side has not, for the microseconds
	 * between the call returning and the loop that counts it - and this poll runs on whichever thread
	 * asked for the switch. What stays put across a second is broken; what moves is that race.
	 */
	private fun checkRefusalChannel() {
		val refused = try {
			WmNative.terrainSectionsRefused.invokeExact() as Int
		} catch (error: Throwable) {
			return
		}

		val (reported, dropped) = try {
			WmNative.terrainSectionsRefusedReported.invokeExact() as Int to
				WmNative.terrainSectionsRefusedDropped.invokeExact() as Int
		} catch (error: Throwable) {
			return
		}

		val missing = reported.toLong() - refusedDrained
		val unaccounted = refused.toLong() - reported - dropped

		if (missing != refusedMissing || unaccounted != refusedUnaccounted) {
			// Numbers that have moved: nothing to say about them until they hold still.
			refusedMissing = missing
			refusedUnaccounted = unaccounted
			refusedReported = false
			return
		}

		if ((missing == 0L && unaccounted == 0L) || refusedReported) {
			return
		}

		refusedReported = true

		WgpuMcMod.LOGGER.warn(
			"wgpu: the refuse-and-resend channel does not add up: the arena refused {} section(s), " +
				"handed over {}, this side drained {}, and {} are unaccounted for",
			refused,
			reported,
			refusedDrained,
			unaccounted,
		)
	}

	/** The offer count the timing report was last written at, so it is sampled like the offer line. */
	private var reportedTiming = 0L

	/** What the last payload measured, for the sampled report below. */
	@Volatile
	private var lastPayloadBytes = 0

	/** What Rust has been told about each section, so the next rebuild can send only what changed. */
	private val sent = ConcurrentHashMap<Long, Sent>()

	/**
	 * Which world [sent] describes, as the native side last numbered it.
	 *
	 * Every record in a payload is stamped with this, and the native side refuses a record stamped
	 * with anything but its own number: a chunk build that was already running when the player changed
	 * dimension would otherwise install the old world's blocks under the new world's coordinates, and
	 * - because it would be recorded as sent - they would never be offered again.
	 *
	 * The number is the *native* side's, adopted in [forgetAll] from the call that bumps it: one
	 * counter with one owner, so the two sides cannot drift into a state where every payload is
	 * refused.
	 */
	@Volatile
	private var generation = 0

	/**
	 * Forgets everything this side has told Rust about the world, for a level change.
	 *
	 * Called from `LevelRenderer#setLevel` - a new world, a dimension change, and `setLevel(null)`
	 * when the player goes back to the title screen all arrive there - and the native side is asked to
	 * do the same (`clearSections`), which is also where [generation] comes from.
	 *
	 * Without this, the two sides keep describing the world that was left behind: Rust would go on
	 * baking against its cache, this side would go on believing sections it had sent are there, and
	 * nothing rebuilds a section whose blocks have not changed - so the new world would draw the old
	 * world's ground at the coordinates the two happen to share.
	 */
	@JvmStatic
	fun forgetAll(generation: Int) {
		sent.clear()
		this.generation = generation

		WgpuMcMod.LOGGER.info(
			"wgpu: the section bake was forgotten on both sides (generation {})",
			generation,
		)
	}

	/**
	 * Offers the middle section of [region] to the Rust baker. Called from the head of the compile
	 * task, on the chunk-build worker thread.
	 *
	 * What is handed over is the data and only the data: Rust copies what it keeps out of the payload
	 * before the call returns, and the meshing itself runs on Rust's own thread pool. The counters
	 * below therefore count *offers*, while the `wgpu-mc: baked ...` lines in the log are the bakes
	 * themselves.
	 *
	 * A failure is logged and swallowed: until the Rust side draws these sections, the game still has
	 * Minecraft's mesh, and taking the world down over an optimisation that is not drawing anything
	 * yet would be the wrong trade.
	 */
	@JvmStatic
	fun bake(region: RenderSectionRegion) {
		if (!isOn()) return

		// A rebuild can arrive before the native side has cached block states - the cache is built on
		// the title screen, and a quickplay launch is loading chunks well before that finishes. A bake
		// then would find no registry and no "air", and the whole copy would be wasted.
		if (!WgpuNative.blocksCached()) return

		try {
			bakeNow(region)
		} catch (throwable: Throwable) {
			WgpuMcMod.LOGGER.error("wgpu: the Rust terrain bake failed", throwable)
		}
	}

	/**
	 * The block states of one of the 27 sections, from the rebuild's own snapshot.
	 *
	 * The snapshot is what Minecraft is meshing the same section from, so the two meshes agree, and a
	 * `SectionCopy` holds a *copy* of the container - reading it off-thread is safe, where a live
	 * `LevelChunkSection` is guarded by a threading detector.
	 *
	 * When the snapshot cannot be reached at all (`ContainerData` says so once, with the reason), the
	 * live chunk is read instead: same sections, keyed the same way, a moment newer than the mesh
	 * beside it. Reading the world is the fallback rather than the plan because it is the one thing
	 * here that is not the data the game itself is baking from.
	 */
	private fun statesOf(
		region: RenderSectionRegion,
		copies: Array<Any?>?,
		index: Int,
		x: Int,
		y: Int,
		z: Int,
	): PalettedContainer<BlockState>? {
		if (copies != null) {
			val copy = copies.getOrNull(index) ?: return null
			return (copy as? SectionCopyAccessor)?.`wgpu_mc$states`()
		}

		val level = Minecraft.getInstance().level ?: return null
		val chunk = level.chunkSource.getChunkNow(x, z) ?: return null
		val sectionIndex = chunk.getSectionIndexFromSectionY(y)
		if (sectionIndex < 0 || sectionIndex >= chunk.sectionsCount) return null

		val section = chunk.getSection(sectionIndex)
		return if (section.hasOnlyAir()) null else section.states
	}

	private fun bakeNow(region: RenderSectionRegion) {
		val corner = region as RenderSectionRegionAccessor
		val minX = corner.`wgpu_mc$minSectionX`()
		val minY = corner.`wgpu_mc$minSectionY`()
		val minZ = corner.`wgpu_mc$minSectionZ`()
		val targetX = minX + 1
		val targetY = minY + 1
		val targetZ = minZ + 1

		// Rust keeps the light of every section it has been told about and drops the ones far from the
		// player, so a section this side counts as sent can be gone there. It says so instead of baking
		// against holes, and the answer is to forget what was sent and hand the whole neighbourhood
		// over again - once, because the second call carries everything the first one was missing.
		if (send(region, minX, minY, minZ, targetX, targetY, targetZ, force = false)) {
			resyncs++
			sent.clear()
			send(region, minX, minY, minZ, targetX, targetY, targetZ, force = true)
		}

		if (sent.size > SENT_LIMIT) {
			// Rust keeps what it has; dropping this side's bookkeeping only means the next payloads
			// carry more than they had to.
			sent.clear()
		}

		bakes++
		if (Diagnostics.loggingEnabled() || bakes - reported >= REPORT_EVERY) {
			reported = bakes
			WgpuMcMod.LOGGER.info(
				"wgpu: offered section ({}, {}, {}) to the Rust baker with {} B of payload ({} offer(s), {} resync(s) so far)",
				targetX,
				targetY,
				targetZ,
				lastPayloadBytes,
				bakes,
				resyncs,
			)
		}

		// The same numbers the F3 line shows, in the log: the switch is about one run's cost, and a
		// run whose F3 screen nobody is looking at should still be able to answer it.
		if (Diagnostics.sectionTimingEnabled()) {
			val offers = WgpuMcMod.SECTION_OFFERS.sum()
			if (offers > 0 && bakes - reportedTiming >= REPORT_EVERY) {
				reportedTiming = bakes
				WgpuMcMod.LOGGER.info(
					"wgpu: section feed per offer over {} offer(s): light {} ns, blocks {} ns, call {} ns, {} B of payload",
					offers,
					WgpuMcMod.TIME_SPENT_SECTION_LIGHT.sum() / offers,
					WgpuMcMod.TIME_SPENT_SECTION_BLOCKS.sum() / offers,
					WgpuMcMod.TIME_SPENT_SECTION_CALL.sum() / offers,
					WgpuMcMod.SECTION_PAYLOAD_BYTES.sum() / offers,
				)
			}
		}
	}

	/**
	 * Builds the payload for one section rebuild and hands it over.
	 *
	 * Returns Rust's answer: `true` when it is missing part of the neighbourhood this call described,
	 * which is the caller's cue to send everything again. `force` writes every one of the 27 sections
	 * regardless of what this side believes Rust already has.
	 *
	 * The bookkeeping is committed only once the call has returned. That is not bookkeeping for its own
	 * sake: chunk builds run on several threads at once, so a section this thread has just written into
	 * its payload is a section a *second* thread may already be describing as "you have this one" -
	 * and until the call lands, it is not true. Committing late is what keeps a mask from claiming a
	 * section Rust has never seen, which is a resync, which is the whole neighbourhood sent twice.
	 */
	private fun send(
		region: RenderSectionRegion,
		minX: Int,
		minY: Int,
		minZ: Int,
		targetX: Int,
		targetY: Int,
		targetZ: Int,
		force: Boolean,
	): Boolean {
		val lightEngine = region.lightEngine
		val blockLayers = lightEngine.getLayerListener(LightLayer.BLOCK)
		val skyLayers = lightEngine.getLayerListener(LightLayer.SKY)
		val copies = ContainerData.sectionCopies(region)

		var knownBlocks = 0
		var knownLight = 0
		var present = 0

		// The clock is read only while the switch is on, and each phase is timed around the work it
		// names: the light of the 27 sections, the block data out of the container (palette, storage
		// and the hash that decides whether it has to be sent), and the call itself. The payload
		// writes belong to the phase whose data they carry, so the three add up to the loop plus the
		// call rather than to a fourth "marshalling" phase nobody can act on.
		val timing = Diagnostics.sectionTimingEnabled()

		var lightNanos = 0L
		var blockNanos = 0L

		// The sections this call changes, applied to [sent] after it returns.
		val updates = ArrayList<Update>(SECTIONS)

		val payload = Payload.ofThread()
		payload.begin()

		// One read for the whole payload: every record in it describes the same world. A level change
		// while it is being written is exactly what the stamp is for - the records that were already
		// written for the old world are refused by the other side rather than applied.
		val stamp = generation

		for (index in 0 until SECTIONS) {
			// x fastest, then y, then z - the order the Rust provider indexes, and the order the region
			// keeps its own section copies in.
			val x = minX + index % AXIS
			val y = minY + (index / AXIS) % AXIS
			val z = minZ + index / (AXIS * AXIS)
			val key = SectionPos.asLong(x, y, z)

			val lightStarted = if (timing) System.nanoTime() else 0L
			val light = lightOf(blockLayers, skyLayers, x, y, z)
			if (timing) lightNanos += System.nanoTime() - lightStarted

			val blocksStarted = if (timing) System.nanoTime() else 0L
			val states = statesOf(region, copies, index, x, y, z)
			val blocks = if (states == null) null else describe(states)
			val entry = sent[key]

			if (blocks != null) {
				present++
			}

			val blocksChanged = force || blocks != entry?.blocks
			val lightChanged = force || light != entry?.light

			// "This section has no blocks" is said *every* time rather than only when it changed: a
			// payload that says nothing about a slot leaves whatever the other side already had for
			// it, so a section that was unloaded - or that came back as air - would go on being meshed
			// from the copy Rust kept. Nobody would notice: the stale data is blocks, and the next
			// rebuild would compare against it and find nothing to say.
			if (blocks == null) {
				payload.writeAbsentBlock(index, stamp)
			} else if (blocksChanged) {
				payload.writeBlock(index, blocks, stamp)
			} else {
				// Rust has this section and this call does not carry it.
				knownBlocks = knownBlocks or (1 shl index)
			}

			if (timing) blockNanos += System.nanoTime() - blocksStarted

			if (light == null) {
				payload.writeAbsentLight(index, stamp)
			} else if (lightChanged) {
				payload.writeLight(index, light, stamp)
			} else {
				knownLight = knownLight or (1 shl index)
			}

			// What this call told the other side about the section, including the nothings. Dropping
			// the entry instead - "nothing to say" and "remember nothing" are not the same thing -
			// would make the next rebuild compare `null` against a missing entry and conclude there
			// was nothing to say, which is how the stale blocks above stay stale.
			if (blocksChanged || lightChanged || blocks == null || light == null) {
				updates.add(Update(index, key, Sent(blocks, light)))
			}
		}

		payload.finish(knownBlocks, knownLight)

		lastPayloadBytes = payload.length

		if (Diagnostics.loggingEnabled()) {
			WgpuMcMod.LOGGER.info(
				"wgpu: sent section ({}, {}, {}): {} B of payload, {} of {} sections present, {} block and {} light record(s)",
				targetX,
				targetY,
				targetZ,
				payload.length,
				present,
				SECTIONS,
				payload.blocks,
				payload.lights,
			)
		}

		val callStarted = if (timing) System.nanoTime() else 0L
		val answer = WgpuNative.bakeSections(targetX, targetY, targetZ, payload.address, payload.length)
		val callNanos = if (timing) System.nanoTime() - callStarted else 0L

		// Two answers in one word: which records the other side refused - written for a world it has
		// since forgotten - and whether it is missing part of the neighbourhood, which is the older
		// cue to send everything again.
		val rejected = answer and REJECTED_MASK
		val resync = answer and RESYNC != 0

		if (timing) {
			WgpuMcMod.TIME_SPENT_SECTION_LIGHT.add(lightNanos)
			WgpuMcMod.TIME_SPENT_SECTION_BLOCKS.add(blockNanos)
			WgpuMcMod.TIME_SPENT_SECTION_CALL.add(callNanos)
			WgpuMcMod.SECTION_PAYLOAD_BYTES.add(payload.length.toLong())
			// One offer per call: a resync sends twice, and the second send is part of the same
			// rebuild's cost rather than a rebuild of its own.
			if (!force) {
				WgpuMcMod.SECTION_OFFERS.increment()
			}
		}

		if (!resync) {
			for (update in updates) {
				// A refused record was written for a world that is gone, so it says nothing about the
				// one the other side is describing now: keeping it out of the table is what makes the
				// next rebuild of that section carry its data again, rather than leaving a hole the
				// two sides both believe was filled.
				if (rejected and (1 shl update.index) != 0) {
					continue
				}

				sent[update.key] = update.entry
			}
		}

		return resync
	}

	/** The two light layers of a section, or `null` when neither is loaded. */
	private fun lightOf(
		blockLayers: net.minecraft.world.level.lighting.LayerLightEventListener,
		skyLayers: net.minecraft.world.level.lighting.LayerLightEventListener,
		x: Int,
		y: Int,
		z: Int,
	): Light? {
		val pos = SectionPos.of(x, y, z)
		val block = blockLayers.getDataLayerData(pos)?.data
		val sky = skyLayers.getDataLayerData(pos)?.data

		if (block == null && sky == null) {
			return null
		}

		return Light(block ?: EMPTY_LIGHT, sky ?: EMPTY_LIGHT)
	}

	/**
	 * Reads one snapshot section into the shape the cache compares and the payload writes.
	 *
	 * The palette and the storage come out of the container through [ContainerData] rather than the
	 * public API: `pack` would hand them over but re-encodes the section first, and `getAll` is a
	 * lambda per position - both are the per-rebuild cost this path exists to remove. `null` means
	 * this build cannot reach them at all, which leaves the section out rather than guessing.
	 */
	private fun describe(states: PalettedContainer<BlockState>): Blocks? {
		val storage = ContainerData.storage(states)
		val palette = ContainerData.palette(states)

		if (palette == null || storage == null) {
			return null
		}

		// One fluid byte per palette entry, beside the keys: a state's fluid is not in its model,
		// so this is the only thing that says a section holds lava - and how deep it is there.
		val fluids = ByteArray(palette.size) { index -> fluidByte(palette.valueFor(index)) }

		sectionsDescribed.incrementAndGet()
		for (fluid in fluids) {
			when (fluid.toInt() and 0b11) {
				1, 2 -> paletteFluids.incrementAndGet()
				else -> continue
			}

			if (fluid.toInt() and 0b11 == 2) {
				paletteLava.incrementAndGet()
			}
		}

		val table = IntArray(palette.size) { index ->
			val state = palette.valueFor(index)
			val key = (state as? RustBlockStateAccessor)?.`wgpu_mc$getRustBlockStateIndex`() ?: 0

			key
		}

		// A section of one state has no storage of its own (`ZeroBitStorage`): every position is index
		// 0, which is the single entry the palette holds. One zero long and a zero mask is what makes
		// the Rust decode agree with that.
		if (storage !is SimpleBitStorage) {
			return Blocks(Table(table), fluids, 0, longArrayOf(0L), 0, 0L, 0, 0, 0, 4096)
		}

		val geometry = storage as PackedIntegerArrayMixin

		return Blocks(
			Table(table),
			fluids,
			storage.bits,
			storage.raw,
			geometry.`wgpu_mc$valuesPerLong`(),
			geometry.`wgpu_mc$mask`(),
			geometry.`wgpu_mc$divideMul`(),
			geometry.`wgpu_mc$divideAdd`(),
			geometry.`wgpu_mc$divideShift`(),
			storage.size,
		)
	}

	/**
	 * The fluid a state carries, packed into one byte: kind in the low two bits, MC's
	 * `FluidState#getAmount` in the next four.
	 *
	 * A fluid is not in its block model - lava and water have no model elements at all - so this is
	 * the only thing that tells the Rust mesher that a section holds lava, and how deep the fluid is
	 * at that position: MC's own `getOwnHeight` is `amount / 9`, so the surface height is the same
	 * arithmetic on both sides. Kinds are 1 water, 2 lava, 3 anything else, which the mesher leaves to
	 * Minecraft rather than drawing with a texture it does not have.
	 */
	private fun fluidByte(state: BlockState?): Byte {
		if (state == null) return 0

		val fluid = state.fluidState
		if (fluid.isEmpty) return 0

		val kind = when {
			fluid.type.`isSame`(Fluids.WATER) -> 1
			fluid.type.`isSame`(Fluids.LAVA) -> 2
			else -> 3
		}

		return (kind or (fluid.amount shl 2)).toByte()
	}

	/** Sections described for the Rust baker this run, and the fluid bytes that went with them. */
	private val sectionsDescribed = java.util.concurrent.atomic.AtomicLong()

	private val paletteFluids = java.util.concurrent.atomic.AtomicLong()

	private val paletteLava = java.util.concurrent.atomic.AtomicLong()

	/**
	 * Diagnostics: what the fluid bytes have said so far, for the line the terrain pass logs.
	 *
	 * The Rust mesher counts the fluid blocks it is *handed*, and this counts the fluid the payload
	 * *carried* - the two numbers together say which side of the bridge a fluid that never appears went
	 * missing on. Neither is visible from the other side of the bridge, which is why both are printed.
	 */
	@JvmStatic
	val fluidDiagnostics: String
		get() = "$sectionsDescribed section(s) described carrying $paletteFluids fluid palette " +
			"entr(ies), $paletteLava of them lava"
	/**
	 * The palette translation table: `keys[minecraft index]` is the Rust block key.
	 *
	 * Compared by content and hashed, so a section whose palette did not change costs one integer
	 * comparison.
	 */
	private class Table(val keys: IntArray) {
		private val hash = hashInts(keys)

		override fun equals(other: Any?): Boolean =
			other is Table && hash == other.hash && keys.contentEquals(other.keys)

		override fun hashCode(): Int = hash

		override fun toString(): String = "Table(${keys.size})"
	}

	/** One section's block storage, exactly as Minecraft holds it. */
	private class Blocks(
		val palette: Table,
		val fluids: ByteArray,
		val bits: Int,
		val longs: LongArray,
		val valuesPerLong: Int,
		val mask: Long,
		val divideMul: Int,
		val divideAdd: Int,
		val divideShift: Int,
		val size: Int,
	) {
		private val hash = hashLongs(longs)

		override fun equals(other: Any?): Boolean =
			other is Blocks &&
				hash == other.hash &&
				bits == other.bits &&
				palette == other.palette &&
				longs.contentEquals(other.longs)

		override fun hashCode(): Int = hash
	}

	/** One section's two light layers. */
	private class Light(val block: ByteArray, val sky: ByteArray) {
		private val blockHash = hashBytes(block)
		private val skyHash = hashBytes(sky)

		override fun equals(other: Any?): Boolean =
			other is Light &&
				blockHash == other.blockHash &&
				skyHash == other.skyHash &&
				block.contentEquals(other.block) &&
				sky.contentEquals(other.sky)

		override fun hashCode(): Int = blockHash * 31 + skyHash
	}

	/** What this side last told Rust about one section. `null` means "it is not there". */
	private class Sent(val blocks: Blocks?, val light: Light?)

	/**
	 * One entry a call changes in [sent], kept with the payload slot it was written at.
	 *
	 * The slot is what the other side's answer names: a record it refused - because the world was
	 * replaced between this payload being written and being read - is one this side must not record as
	 * sent, and it is the slot, not the section key, that the answer is about.
	 */
	private class Update(val index: Int, val key: Long, val entry: Sent)

	/**
	 * Hashes rather than the identities of the objects the data came from.
	 *
	 * Minecraft mutates a `PalettedContainer`'s storage in place - `SimpleBitStorage#set` writes into
	 * the same longs, `createOrReuseData` reuses the same `Data` while the palette still fits, and the
	 * light engines write into the same `DataLayer` byte arrays - so "the same object" does not mean
	 * "the same contents", and a rebuild happens *because* something changed. Comparing the bytes is
	 * exact, and it is a scan of a few kilobytes against the 4096 palette lookups, the bit-packing and
	 * the 54 array allocations this replaced.
	 */
	private fun hashLongs(values: LongArray): Int {
		var hash = -0x7ee3623b
		for (value in values) {
			hash = (hash xor value.toInt()) * 0x01000193
			hash = (hash xor (value ushr 32).toInt()) * 0x01000193
		}
		return hash
	}

	private fun hashInts(values: IntArray): Int {
		var hash = -0x7ee3623b
		for (value in values) {
			hash = (hash xor value) * 0x01000193
		}
		return hash
	}

	private fun hashBytes(values: ByteArray): Int {
		var hash = -0x7ee3623b
		for (value in values) {
			hash = (hash xor value.toInt()) * 0x01000193
		}
		return hash
	}

	/**
	 * The payload buffer, one per build thread.
	 *
	 * Per thread because the chunk build runs on several at once; reused because a payload is written
	 * for every rebuild of every section, and allocating a few hundred kilobytes each time is the kind
	 * of cost this path exists to remove.
	 */
	private class Payload private constructor() {
		private val arena = Arena.ofShared()
		private val segment: MemorySegment = arena.allocate(PAYLOAD_BYTES)
		private var cursor = BLOBS_AT

		var blocks = 0
			private set
		var lights = 0
			private set
		var length = 0
			private set

		val address: Long get() = segment.address()

		fun begin() {
			cursor = BLOBS_AT
			blocks = 0
			lights = 0
			length = 0
		}

		/** A section Rust should keep, or replace what it had. */
		fun writeBlock(index: Int, section: Blocks, generation: Int) {
			val at = blocksAt(blocks)

			val paletteOffset = cursor
			put(paletteOffset, section.palette.keys)
			cursor += section.palette.keys.size * 4L

			// The longs are read as 64-bit values on the other side, so they start on an 8-byte
			// boundary: not because anything requires it, but because a misaligned read is the kind of
			// thing that is fast on one machine and slow on the next.
			cursor = (cursor + 7L) / 8L * 8L
			val longsOffset = cursor
			put(longsOffset, section.longs)
			cursor += section.longs.size * 8L

			val fluidsOffset = cursor
			put(fluidsOffset, section.fluids)
			cursor += section.fluids.size

			word(at, index)
			word(at + 4, 1)
			word(at + 8, section.bits)
			word(at + 12, section.size)
			word(at + 16, section.valuesPerLong)
			word(at + 20, section.mask.toInt())
			word(at + 24, (section.mask ushr 32).toInt())
			word(at + 28, section.divideMul)
			word(at + 32, section.divideAdd)
			word(at + 36, section.divideShift)
			word(at + 40, section.palette.keys.size)
			word(at + 44, paletteOffset.toInt())
			word(at + 48, section.longs.size)
			word(at + 52, longsOffset.toInt())
			word(at + 56, fluidsOffset.toInt())
			word(at + BLOCK_RECORD_GENERATION_WORD * 4, generation)

			blocks++
		}

		/** A section Rust should forget: it is air, or it is not loaded any more. */
		fun writeAbsentBlock(index: Int, generation: Int) {
			val at = blocksAt(blocks)
			word(at, index)
			word(at + 4, 0)
			word(at + BLOCK_RECORD_GENERATION_WORD * 4, generation)
			blocks++
		}

		fun writeLight(index: Int, light: Light, generation: Int) {
			val at = lightAt(lights)

			val blockOffset = cursor
			put(blockOffset, light.block)
			cursor += LIGHT_BYTES

			val skyOffset = cursor
			put(skyOffset, light.sky)
			cursor += LIGHT_BYTES

			word(at, index)
			word(at + 4, blockOffset.toInt())
			word(at + 8, skyOffset.toInt())
			word(at + LIGHT_RECORD_GENERATION_WORD * 4, generation)

			lights++
		}

		/**
		 * A section Rust should forget the light of - one that is not loaded at all, and whose light
		 * is therefore nothing rather than dark. Both blob offsets are zero, which no written record
		 * can be: the blobs start after the records.
		 */
		fun writeAbsentLight(index: Int, generation: Int) {
			val at = lightAt(lights)
			word(at, index)
			word(at + 4, 0)
			word(at + 8, 0)
			word(at + LIGHT_RECORD_GENERATION_WORD * 4, generation)
			lights++
		}

		fun finish(knownBlocks: Int, knownLight: Int) {
			word(0, MAGIC)
			word(4, 0)
			word(8, blocks)
			word(12, lights)
			word(16, knownBlocks)
			word(20, knownLight)
			length = cursor.toInt()
		}

		private fun blocksAt(count: Int) = HEADER_BYTES + min(count, SECTIONS) * BLOCK_RECORD_BYTES

		private fun lightAt(count: Int) =
			HEADER_BYTES + SECTIONS * BLOCK_RECORD_BYTES + min(count, SECTIONS) * LIGHT_RECORD_BYTES

		private fun word(at: Long, value: Int) {
			segment.set(ValueLayout.JAVA_INT, at, value)
		}

		private fun put(at: Long, values: IntArray) {
			if (values.isEmpty()) return
			segment.asSlice(at, values.size * 4L).copyFrom(MemorySegment.ofArray(values))
		}

		private fun put(at: Long, values: LongArray) {
			if (values.isEmpty()) return
			segment.asSlice(at, values.size * 8L).copyFrom(MemorySegment.ofArray(values))
		}

		private fun put(at: Long, values: ByteArray) {
			if (values.isEmpty()) return
			segment.asSlice(at, values.size.toLong()).copyFrom(MemorySegment.ofArray(values))
		}

		companion object {
			private val perThread = ThreadLocal.withInitial { Payload() }

			fun ofThread(): Payload = perThread.get()
		}
	}
}
