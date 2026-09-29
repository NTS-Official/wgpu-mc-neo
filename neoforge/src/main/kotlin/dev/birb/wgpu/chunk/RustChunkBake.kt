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

		// And the claim against the fact, on the same clock and for the same reason - see the note on
		// `reportClaimAgainstArena`, which is the measurement a hole needs to be attributable at all.
		reportClaimAgainstArena()

		if (enabled == previous && reportedState) {
			return
		}

		reportedState = true

		WgpuMcMod.LOGGER.info(
			"wgpu: the Rust terrain path is {} (the renderer's `{}` setting)",
			if (enabled) "on" else "off",
			SETTING,
		)

		// **Both directions rebuild the world, and they have to.** Turning the path *on* is the obvious
		// half: the graph can only draw what the baker has baked, so every section has to be offered to
		// it. Turning it *off* is the half that was missing while Minecraft's meshes were being built
		// anyway - they were the fallback, so nothing had to be done to get them back.
		//
		// And they are not being built any more: with the path on, a section Rust took is a section whose
		// Minecraft mesh was dropped before it was uploaded (`SectionCompilerMixin`). So the sections in
		// the view are holding empty meshes, and turning the switch off without this leaves the world
		// drawn by nobody - the graph is off and Minecraft has nothing to draw.
		//
		// `allChanged` is what marks every section dirty and rebuilds it, and it is the same call a
		// resource reload makes. The sections are meshed over the next second or two, which is the same
		// window the `on` direction already had. Only a real *change* reaches here - see the early return
		// above - so a session that never touches the switch never pays for it.
		Minecraft.getInstance().execute { Minecraft.getInstance().levelRenderer.allChanged() }
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
	 * Dropping the entry is not enough on its own, and that is the half this used to be missing: it
	 * makes the *next* rebuild carry the blocks, and the next rebuild is the thing that does not
	 * happen. A section whose mesh is missing is a section Minecraft believes it has meshed, so nothing
	 * dirties it - the arena stays full, the section stays a hole, and the player breaking a block in
	 * it changes nothing they can see. So the section is marked dirty here as well, which is what asks
	 * the game for that rebuild. The cost is one rebuild per refusal, and the native side grows the
	 * arena on the same refusal (see `WmRenderer::grow_arena`), so the second attempt has room.
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

		// **The rebuild is asked for whether or not the arena can grow, and that is the fix for a hole
		// that stayed after the queue was added.** This used to read `if (canGrow && ...)`, on the
		// argument that a rebuild for a section the arena cannot hold is a spin: the arena doubles on a
		// refusal and stops at the device's buffer limit, so past that point a rebuild per refusal is
		// work that cannot converge.
		//
		// That argument is right about the spin and wrong about the alternative. A key dropped here has
		// already been removed from [rustHas] a line above, so Minecraft's mesh for that section is no
		// longer suppressed - but *nothing asks for it to be rebuilt either*, and a mesh that is not
		// suppressed and not rebuilt is the same 16x16x16 hole by a different route. It is the bug the
		// queue was added to fix, reintroduced by the gate that was meant to keep the queue cheap.
		//
		// Asking for the rebuild converges even when the arena is full, because of what the rebuild
		// does: it bakes, the bake fails to allocate, the refusal clears `rustHas` again, and the
		// **bake answers "not taken"** - so `noteTookSection(false)` leaves Minecraft's own mesh in
		// place. The section ends up drawn by the game, which is the correct answer for a section this
		// side has no room for. The cost is one rebuild per refused section, and the rate is [budget]'s
		// in [redirtyDue], which is where a rate belongs.
		var queued = 0

		for (key in refused) {
			sent.remove(key)
			// And a refused section is one Rust is not drawing, so Minecraft's mesh goes back to being
			// the fallback for it. The rebuild `redirtyDue` asks for is what puts it back.
			rustHas.remove(key)

			if (pendingRedirty.size < PENDING_REDIRTY_LIMIT) {
				pendingRedirty[key] = System.nanoTime()
				queued++
			}
		}

		refusedDrained += refused.size

		// Whether the arena can still grow is no longer a gate here, but it is the number that says
		// whether this run is at the device's buffer limit - which is the state where the sections
		// being refused will end up drawn by Minecraft instead of by this side.
		val canGrow = try {
			WmNative.terrainArenaCanGrow.invokeExact() as Boolean
		} catch (error: Throwable) {
			true
		}

		WgpuMcMod.LOGGER.warn(
			"wgpu: the section arena refused {} section(s) (it {} grow); forgetting them, so the next " +
				"rebuild of each carries its blocks again, and queueing {} of those rebuild(s) ({} " +
				"drained so far, {} waiting)",
			refused.size,
			if (canGrow) "can" else "cannot",
			queued,
			refusedDrained,
			pendingRedirty.size,
		)
	}

	/**
	 * The sections waiting for a rebuild, as `SectionPos.asLong` keys.
	 *
	 * **A queue rather than a burst, and this is the half that used to lose sections.** The drain used to
	 * mark at most [REDIRTY_PER_TICK] of a refusal batch dirty and drop the rest - and a dropped one was
	 * not merely delayed: its [sent] record had already been removed and nothing would offer that section
	 * again, so it stayed a hole until the player broke a block in it. The native side hands each refusal
	 * over exactly once (`SectionStorage::refused` is a `mem::take`), so there is no second copy coming.
	 *
	 * A `LinkedHashSet` because a section refused twice before it was retried is one rebuild and not two:
	 * the insert is the de-duplication, and its order is the order the refusals arrived in.
	 *
	 * Bounded, and the bound is the native side's own refusal cap: past that it has already forgotten
	 * refusals of its own, so a queue that kept growing would hold entries for sections that were never
	 * coming back.
	 */
	private val pendingRedirty = java.util.LinkedHashMap<Long, Long>()

	/** The longest a section has waited for its rebuild, in nanoseconds. See [redirtyDue]. */
	private var oldestRedirtyNanos = 0L

	/**
	 * How many refusals may wait for a rebuild at once.
	 *
	 * The native side drops refused positions past its own cap (4096) before this side ever sees them, so
	 * a queue that grew without limit would be holding entries whose sections were already forgotten
	 * there. The budget is [REDIRTY_PER_FRAME] a frame, so even this many is drained in seconds - and the
	 * number is only reached when the arena is refusing faster than the game can rebuild, which is the
	 * case the cap turns into "stop queueing" rather than "grow without bound".
	 */
	private const val PENDING_REDIRTY_LIMIT = 4096

	/** How many refusals are waiting for a rebuild. For the report. */
	@JvmStatic
	fun pendingRedirtyCount(): Int = pendingRedirty.size

	/**
	 * Reports what this side claims Rust has against what Rust is actually drawing, once a second.
	 *
	 * **This is the measurement three rounds of reasoning went without.** [rustHas] means "this side has
	 * told Rust about the section", and the mixin that drops Minecraft's mesh reads that same claim - so a
	 * section that was told and never *published* is drawn by neither renderer, and it is a 16x16x16 hole
	 * with nothing in the logs to say so. Nothing compared the claim with the fact until this line.
	 *
	 * The two numbers are not expected to be equal, and that is not a broken invariant: [rustHas] also
	 * holds sections the arena has legitimately trimmed, because this side's bookkeeping is the game's
	 * whole view while the arena is only what is being drawn. What matters is whether the gap moves - a
	 * gap that grows while the player stands still is sections being claimed and not published, and that
	 * is the hole.
	 *
	 * Called with the settings poll. Once a second while the logging switch is on, and **once every ten
	 * seconds even when it is off** - because a switch that hides the only line able to describe a hole
	 * is a trap: that is exactly how a round of this investigation was spent with the report "missing",
	 * when it was the `logging` switch that was off. One line every ten seconds is not noise, and a run
	 * that is looking at holes should never be blind.
	 */
	@JvmStatic
	fun reportClaimAgainstArena() {
		val now = System.nanoTime()

		val interval = if (Diagnostics.loggingEnabled()) {
			1_000_000_000L
		} else {
			10_000_000_000L
		}

		if (now - claimReportedAt < interval) {
			return
		}

		claimReportedAt = now

		val arena = try {
			WgpuNative.arenaSections()
		} catch (error: Throwable) {
			return
		}

		WgpuMcMod.LOGGER.info(
			"wgpu: this side claims Rust has {} section(s); the arena is drawing {}, with {} awaiting a " +
				"rebuild and {} bake(s) queued. Rebuilds: {} meshed by Rust's answer, {} refused by it, " +
				"{} left to Minecraft because the arena is full, {} left to it because Rust was never told",
			rustHas.size,
			arena,
			pendingRedirty.size,
			try {
				WgpuNative.queuedBakes()
			} catch (error: Throwable) {
				-1
			},
			suppressed,
			refusedByRust,
			refusedAtCapacity,
			firstLookCount,
		)

		// **Which atlas the faces baked in the last second went to**, which is the one thing about the
		// atlas routing that cannot be seen: both atlases are 2048x2048 and answer to the same filters, so
		// a face on the wrong one is not drawn differently - it samples a mip chain built from the whole
		// packed sheet rather than per sprite, which is a blurred, half-transparent block. A large second
		// number is the routing not working, and it is the reason a block can be blurry while every other
		// explanation has been ruled out.
		try {
			val faces = WgpuNative.atlasFaceCounts()

			if (faces.isNotEmpty()) {
				WgpuMcMod.LOGGER.info("wgpu: faces baked since the last report: {}", faces)
			}
		} catch (error: Throwable) {
			// A diagnostic that cannot be read is not worth taking the game down for.
		}

		// **And how long a hole lasted**, which is the number a run that heals its own holes needs.
		//
		// A section that was refused, dropped or trimmed is not drawn until the rebuild asked for here
		// reaches it, so the age of the oldest queued entry *is* how long that hole was on screen. It
		// says whether the queue is keeping up - milliseconds, which nobody sees - or whether the player
		// is looking at the holes for a second or more, which is a rate rather than a correctness
		// problem, and a rate is a constant.
		if (oldestRedirtyNanos > 0) {
			WgpuMcMod.LOGGER.info(
				"wgpu: the longest a section has waited for its rebuild is {} ms",
				oldestRedirtyNanos / 1_000_000,
			)
		}
	}

	/** How many rebuilds were left to Minecraft because Rust refused the payload. See [bakeNow]. */
	private var refusedByRust = 0L

	/** The slot of the 27 the rebuild is about. Must match `CENTER` in `wgpu-mc-jni/src/section.rs`. */
	private const val CENTER = 13

	/**
	 * What one `send` got back, which is two questions and not one.
	 *
	 * `resync` means Rust is missing part of the neighbourhood and the whole of it has to be sent again.
	 * `centreTaken` means the record for the section this rebuild is *about* was accepted - the centre
	 * slot, index [`CENTER`], whose refusal is the only one that leaves this section unbaked.
	 *
	 * They are separate because they were conflated: `send` returned the resync flag alone, so a payload
	 * that was **refused** - the bake queue was full, or the records were written for a world Rust has
	 * forgotten - looked like a successful send to everything downstream. `bakeNow` then answered "Rust
	 * took it" and the mesh was dropped for a section Rust had not taken.
	 */
	private data class Answer(val resync: Boolean, val centreTaken: Boolean)

	/** When [reportClaimAgainstArena] last wrote. */
	private var claimReportedAt = 0L

	/** Rebuilds whose mesh was dropped because Rust was believed to have the section. See [bakeNow]. */
	private var suppressed = 0L

	/** Rebuilds whose mesh was kept because the arena is at its limit. See [bakeNow]. */
	private var refusedAtCapacity = 0L

	/** Rebuilds whose mesh was kept because Rust had never been told about the section. See [bakeNow]. */
	private var firstLookCount = 0L

	/**
	 * Asks the game to rebuild a few of the sections the arena refused, one frame at a time.
	 *
	 * Called every frame rather than every tick, because the two things it is throttled by are both
	 * per-frame: the work it asks for is Minecraft's chunk builds, and the pressure it backs off from is
	 * the bake pool the results go to. A tick is 50 ms of that same work arriving in one lump.
	 *
	 * Three things bound it, and each answers a different way of making this a storm:
	 *
	 *  - **the queue has to be drained**, so a section refused twice is one rebuild;
	 *  - **at most [REDIRTY_PER_FRAME] a frame**, because sixty-four rebuilds of sections whose blocks
	 *    have not changed is a burst the player pays for in frame time. What is skipped is not lost - it
	 *    is still in the queue, and the next frame takes it;
	 *  - **and the bake pool has to have room**, which is the one that matters when the arena is small:
	 *    marking sections dirty makes Minecraft offer them, every offer applies a 27-section payload and
	 *    reserves a bake slot, and a full queue drops the offer - so without this the loop is "queue full,
	 *    mark dirty, offer again, still full" at whatever rate the frames run at. It shrinks the budget
	 *    to one rather than stopping, and [backedUp] says why that difference is load-bearing.
	 *
	 * Returns how many it asked for, for the caller's report.
	 */
	@JvmStatic
	fun redirtyDue(): Int {
		if (pendingRedirty.isEmpty()) {
			return 0
		}

		val client = Minecraft.getInstance()

		// Only with a world loaded: `setSectionDirty` walks the view area, which is not there on the
		// title screen - and after leaving a world the queue can still hold refusals from the one before.
		val renderer = client.levelRenderer ?: return 0

		// **The budget shrinks under backoff, and it never reaches zero.** That distinction is the whole
		// of this function's safety: the queue being fed here is also fed by nothing else, so a backoff
		// that stopped the drain outright would be a livelock - the pool stays full because no rebuild
		// completes, and no rebuild is asked for because the pool is full. Sections would sit in
		// [pendingRedirty] for the session, which is the hole this queue exists to close.
		//
		// One a frame is a trickle rather than a stop: at sixty frames a second it is sixty asks a
		// second against a pool of a few hundred slots, which is slow enough that it cannot be the
		// storm the backoff is for, and it is non-zero, so the pool is always being given work that can
		// finish and make room. See [backedUp] for what "full" means here.
		val budget = if (backedUp()) 1 else REDIRTY_PER_FRAME

		var count = 0
		val iterator = pendingRedirty.entries.iterator()
		var oldest = 0L

		while (iterator.hasNext() && count < budget) {
			val (key, queuedAt) = iterator.next()
			iterator.remove()

			try {
				renderer.setSectionDirty(SectionPos.x(key), SectionPos.y(key), SectionPos.z(key))
			} catch (error: Throwable) {
				// A nudge that does not land is not worth taking the game down for: the section keeps the
				// geometry it had, which is where this path started. Put back, so the next frame tries it
				// again rather than losing it the way the drain used to.
				pendingRedirty[key] = queuedAt
				return count
			}

			// **How long this one waited**, which is how long a hole it made was visible. The queue is
			// ordered, so what comes off first is what has been waiting longest - and the number that
			// matters is not how many are waiting but how stale the oldest is. A few milliseconds is a
			// queue doing its job; a second or more is a hole the player can see and walk towards.
			oldest = maxOf(oldest, System.nanoTime() - queuedAt)
			count++
		}

		if (oldest > oldestRedirtyNanos) {
			oldestRedirtyNanos = oldest
		}

		return count
	}

	/**
	 * Whether the bake pool is behind enough that asking for a lot more work would only make it later.
	 *
	 * A rebuild asked for now arrives at a pool that is already holding [QUEUED_BAKES] bakes, and the
	 * section it belongs to is one the player may have walked away from by the time it is reached. So the
	 * drain slows down: the queue is drained by bakes finishing, and finishing is what makes room. Two
	 * thirds of the pool is the threshold, because below that the pool is working and there is room to
	 * feed it.
	 *
	 * **It slows the drain and does not stop it**, which is the part that matters: this queue is fed by
	 * nothing else, so a backoff that stopped the drain would be a livelock - no rebuild asked for
	 * because the pool is full, and the pool full because no rebuild finishes. Whoever reads this next
	 * and is tempted to make it `return 0`: the sections it would strand are 16x16x16 holes with nothing
	 * drawing them.
	 *
	 * A native call that cannot be made - no renderer yet - reads as "not backed up": a refusal cannot
	 * exist before there is a renderer, so the question is moot rather than dangerous.
	 */
	private fun backedUp(): Boolean = try {
		WgpuNative.queuedBakes() * 3 >= WgpuNative.maxQueuedBakes() * 2
	} catch (error: Throwable) {
		false
	}

	/** How many refused sections one frame may ask the game to rebuild. See [redirtyDue]. */
	private const val REDIRTY_PER_FRAME = 16

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
		// A world change is every section Rust knew about gone, so nothing it was drawing is known
		// either - and Minecraft meshes sections again until Rust has been told about them. See
		// [rustHas].
		rustHas.clear()
		this.generation = generation

		WgpuMcMod.LOGGER.info(
			"wgpu: the section bake was forgotten on both sides (generation {})",
			generation,
		)
	}

	/**
	 * Forgets what this side believes Rust already has, so the next offer of every section carries its
	 * blocks again.
	 *
	 * This is what a **resource reload** needs and what nothing else did. A reload does not change the
	 * blocks, so [send] compares each section's states against what it sent last time, finds them
	 * identical, and carries nothing - while the *models* those states bake to have just been rebuilt
	 * from the new pack, and the vertices sitting in the arena are still the old ones. Nothing else
	 * makes a section stale enough to be offered again: Minecraft only re-meshes what it has thrown
	 * away, and it throws nothing away for a texture pack.
	 *
	 * The generation is deliberately *not* touched. It says which world these sections belong to, and a
	 * reload is the same world - the stamp on every record has to stay the one the native side is
	 * accepting, or every payload of the re-mesh would be refused as the last world's.
	 *
	 * The light goes with the blocks, because the record that remembers it goes with them: a section
	 * re-offered after this is a section whose light may be a frame newer, which is not a cost worth a
	 * second table to avoid.
	 */
	@JvmStatic
	fun forgetSent() {
		val forgotten = sent.size
		sent.clear()

		// The sections themselves have not changed, but what they bake *to* has - a new pack, or a
		// setting that decides how a face is baked - so the arena is holding geometry of the last bake
		// and Minecraft's own mesh is the only correct thing to draw until the re-offers land. So this
		// forgets that Rust is drawing them too, and the re-mesh the caller asks for is one Minecraft
		// takes part in. See [rustHas].
		rustHas.clear()

		WgpuMcMod.LOGGER.info(
			"wgpu: {} section(s) will be offered to the Rust baker with their blocks again",
			forgotten,
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
	/**
	 * Offers one section rebuild to the Rust baker, and answers whether Rust took it.
	 *
	 * The answer is what decides whether Minecraft meshes the same section at all - see [meshesInRust],
	 * which the rebuild task asks before it starts building geometry. So the two halves have to be one
	 * answer: a section Rust did **not** take has to be meshed by Minecraft, or it is a section nothing
	 * draws.
	 *
	 * A failure is logged and answered `false`: the world falls back to Minecraft's mesh for that
	 * section, which is the picture this renderer drew before the path existed, and taking the world
	 * down over an optimisation would be the wrong trade.
	 */
	@JvmStatic
	fun bake(region: RenderSectionRegion): Boolean {
		if (!isOn()) return false

		// A rebuild can arrive before the native side has cached block states - the cache is built on
		// the title screen, and a quickplay launch is loading chunks well before that finishes. A bake
		// then would find no registry and no "air", and the whole copy would be wasted.
		if (!WgpuNative.blocksCached()) return false

		// **Not "did it not throw" - "did Rust take it".** This used to answer `true` for every call
		// that returned normally, which is every call: the early returns inside `bakeNow` are refusals
		// (the bake queue is full, the payload was rejected, the arena is at its limit) and each of them
		// is a plain return. The mixin passes this answer to `noteTookSection`, and *that* is what
		// decides whether Minecraft's mesh is dropped - so a section Rust refused was drawn by neither
		// side, which is the hole. The decision is recorded inside `bakeNow`, beside the tables it
		// depends on; this reads it back rather than inventing a second answer.
		return try {
			bakeNow(region)
			tookThisSection.get()
		} catch (throwable: Throwable) {
			WgpuMcMod.LOGGER.error("wgpu: the Rust terrain bake failed", throwable)
			false
		}
	}

	/**
	 * Whether the rebuild being run on this thread has been **taken** by Rust, so Minecraft's own mesh
	 * for the same section is geometry nothing will ever read. See [rustHas] for what "taken" means.
	 *
	 * **Per thread, and that is the whole reason it is not a field beside the others.** Section rebuilds
	 * run on a pool of chunk-build workers, the bake is called at the head of the task and this is read a
	 * moment later inside the same task, and a single shared flag would be a race the moment two workers
	 * ran two sections at once: one section's bake would answer for the other's read.
	 */
	private val tookThisSection = ThreadLocal.withInitial { false }

	/** For the mixin that drops Minecraft's mesh. See [tookThisSection]. */
	@JvmStatic
	fun meshesInRust(): Boolean = tookThisSection.get()

	/**
	 * Whether the arena is at the device's buffer limit, so a section this side cannot draw is one the
	 * game has to.
	 *
	 * Read on the chunk-build thread, once per rebuild, as a plain native call: the answer changes only
	 * when the arena grows or a world is loaded, and a frame of staleness costs one section's mesh.
	 *
	 * A native call that cannot be made reads as `false` - "not at capacity" - because that is the
	 * answer that drops Minecraft's mesh, and the failure it would otherwise cause is a section drawn
	 * twice rather than a section drawn by neither. The safe direction is the other one, so this is
	 * deliberately the opposite of what a cautious default would be: with no renderer there is no Rust
	 * terrain either, so `bake` has already answered false and this is never consulted.
	 */
	private fun atCapacity(): Boolean = try {
		WgpuNative.terrainArenaAtCapacity()
	} catch (error: Throwable) {
		false
	}

	/**
	 * Whether the arena is in a state where a section this side takes may not be drawable.
	 *
	 * The same question [atCapacity] answers, asked at the one place that wants it *as well as* the
	 * decision: the decision has to be the narrow one - a refusal that is outstanding right now - while
	 * the diagnostic wants to know about the run that refused a moment ago too, because the mesh it
	 * dropped then is still dropped. Two questions, one native answer, and the difference is only which
	 * side of the tick the refusal was drained on.
	 */
	private fun strained(): Boolean = atCapacity()

	/** Records the answer of the bake that just ran on this thread. See [tookThisSection]. */
	@JvmStatic
	fun noteTookSection(took: Boolean) {
		tookThisSection.set(took)
	}

	/**
	 * The sections Rust has been handed at least once - which is what "Rust has this section" means
	 * here, and the only claim this side is entitled to make.
	 *
	 * **A payload leaving is not the same as a mesh existing**, and the difference is a hole in the
	 * world. Rust bakes on its own thread off a queue: `bakeSections` returns as soon as the payload is
	 * copied, and what comes out the other end lands in the arena some frames later, if it lands at all
	 * - the arena refuses sections it has no room for. So a section offered for the first time is a
	 * section Rust may not be drawing yet, and Minecraft's mesh is dropped for such a section is a
	 * section **nothing** draws until something dirties it again, which for a section that is not
	 * changing is never.
	 *
	 * Being handed once closes that: the payload has landed by the next rebuild of the same section by
	 * definition - a rebuild of it is what carries it. So the first rebuild of a section lets Minecraft
	 * mesh it as it always did while Rust catches up, and every rebuild after that drops the mesh.
	 *
	 * The keys are `SectionPos.asLong`, the same key [sent] uses, and the two are cleared together
	 * everywhere: a section this side has forgotten is one Rust may have refused, so it is one
	 * Minecraft's mesh goes back to being the fallback for.
	 */
	private val rustHas: MutableSet<Long> = java.util.concurrent.ConcurrentHashMap.newKeySet()

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
		//
		// `firstLook` is the other answer the call gives, and the two are separate questions: this one
		// says Rust had to be told everything again, that one says it was being told about a section for
		// the first time. See [rustHas].
		val firstLook = BooleanArray(1)

		val first = send(region, minX, minY, minZ, targetX, targetY, targetZ, force = false, firstLook)

		// `accepted` is what Rust took of the payload that decided *this* section, which is the centre
		// slot. See [Answer]: a refusal and a resync are different answers and only one of them means
		// "send everything again".
		var accepted = first.centreTaken

		if (first.resync) {
			resyncs++
			// Both tables go together: this side's bookkeeping is everything it has told Rust, and
			// [rustHas] is the part of that Rust is drawing. A resync means Rust kept less than it was
			// told, so what it is drawing is not known either.
			sent.clear()
			rustHas.clear()
			accepted = send(region, minX, minY, minZ, targetX, targetY, targetZ, force = true, firstLook)
				.centreTaken
		}

		if (sent.size > SENT_LIMIT) {
			// Rust keeps what it has; dropping this side's bookkeeping only means the next payloads
			// carry more than they had to. Both tables go together - see [rustHas].
			sent.clear()
			rustHas.clear()
		}

		// **The answer for this section: `true` only when Rust actually took what was sent.**
		//
		// Three things have to hold, and each of them was a hole on its own at some point:
		//
		//  - the section was not being seen for the first time, which is what makes the *second* rebuild
		//    of a section the one that stops meshing it;
		//  - the arena has room, because a refusal with no room to grow is permanent;
		//  - and the payload was accepted, because `send` can come back having taken nothing - the bake
		//    queue was full and the task was dropped, or the records were refused. The decision was
		//    `true` for all of those while `send` was already reporting them, so a section Rust refused
		//    had Minecraft's mesh dropped too: drawn by neither renderer, and nothing asks for it back
		//    because only an arena refusal reaches the re-offer drain.
		//
		// This is the same mistake as `bake` answering "did it not throw", one layer down. It is read
		// *after* the commit above, which is what makes the tables agree with the answer it gives.
		noteTookSection(!firstLook[0] && !atCapacity() && accepted)

		// **Which of the answers this rebuild got, counted.** The decision has four inputs now and when a
		// hole survives a fix the question is which one is still wrong; `suppressed` is the only one that
		// can leave a hole, so a run where it climbs while holes appear is a run where the other three
		// are not the explanation.
		if (tookThisSection.get()) {
			suppressed++
		} else if (!accepted) {
			refusedByRust++
		} else if (atCapacity()) {
			refusedAtCapacity++
		} else {
			firstLookCount++
		}

		bakes++

		// **Sections whose mesh this side dropped, by position, while the arena is strained.**
		//
		// This is the data four rounds of reasoning went without. Every counter on the report line says
		// *how often* something happened; none of them says *where*, so a hole could not be tied to a
		// decision. This names the positions, so a hole can be stood next to and matched against a line.
		//
		// The condition is the one that can leave a hole rather than the one that decides it. The
		// decision - `noteTookSection(!firstLook[0] && !atCapacity())` - is already false when the arena
		// is strained at the moment of the rebuild, so `tookThisSection` is only true for a section whose
		// decision was made while there was room. The arena can fill *after* that: the bake is queued
		// here and allocates a frame later, so a section suppressed in good faith can be refused when it
		// arrives - and then its mesh is gone and its geometry was never stored. That gap is what this
		// reports, and it is the only remaining way a section ends up drawn by neither renderer.
		//
		// Only while logging is on, so a healthy run's log is unchanged.
		if (Diagnostics.loggingEnabled() && tookThisSection.get() && strained()) {
			WgpuMcMod.LOGGER.warn(
				"wgpu: dropped Minecraft's mesh for section ({}, {}, {}) and the arena is strained now; if " +
					"there is a hole at that position, this rebuild is the decision that made it",
				targetX,
				targetY,
				targetZ,
			)
		}

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
		firstLook: BooleanArray,
	): Answer {
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

			if (!rustHas.contains(key)) {
				firstLook[0] = true
			}

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

				// And Rust has been told about this one, which is what Minecraft's own meshing is gated
				// on. A refusal leaves it unmarked, so the next rebuild of that section still has
				// Minecraft's mesh to fall back on. See [rustHas].
				rustHas.add(update.key)
			}
		}

		// **What the other side actually took, which is a different question from "did the call
		// return".** `resync` means it refused the whole payload; a `rejected` bit means the record for
		// that slot was written against a world it has since forgotten. Either way Rust did not take
		// what was sent, and the caller has to know: it is the caller that decides whether Minecraft's
		// mesh is dropped, and a mesh dropped for a section Rust refused is drawn by neither renderer.
		//
		// The centre slot is the one that decides whether *this* section was taken, because the rebuild
		// is about the centre and a neighbour's refusal only costs the next rebuild an extra payload.
		val centreTaken = !resync && (rejected and (1 shl CENTER)) == 0

		return Answer(resync, centreTaken)
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
	 * `FluidState#getAmount` in the next four, and **which half of the fluid it is** in bit 6.
	 *
	 * A fluid is not in its block model - lava and water have no model elements at all - so this is
	 * the only thing that tells the Rust mesher that a section holds lava, and how deep the fluid is
	 * at that position: MC's own `getOwnHeight` is `amount / 9`, so the surface height is the same
	 * arithmetic on both sides. Kinds are 1 water, 2 lava, 3 anything else, which the mesher leaves to
	 * Minecraft rather than drawing with a texture it does not have.
	 *
	 * **Both halves of each fluid are the same kind here, and that is not a detail.** A fluid is *two*
	 * registered objects - a source and a flowing one, `Fluids.LAVA` and `Fluids.FLOWING_LAVA` - and
	 * `Fluid#isSame` is identity, not "the same kind of fluid":
	 *
	 * ```java
	 * // Fluid
	 * public boolean isSame(Fluid other) { return other == this; }
	 * ```
	 *
	 * `LiquidBlock` builds one fluid state per level and takes the two from different sides
	 * (`stateCache`: level 0 is `fluid.getSource(false)`, levels 1 to 7 are `fluid.getFlowing(8 - level,
	 * false)`, level 8 is `fluid.getFlowing(8, true)` - the falling state), so **a flowing block answers
	 * `FLOWING_LAVA`**, and `FLOWING_LAVA.isSame(LAVA)` is false.
	 *
	 * Classified against the source alone, every flowing block came out as kind 3 - "a fluid this
	 * mesher does not know" - and the mesher *skips* those. The picture that makes: a lava lake drawn
	 * only where it is still, every lava *fall* drawn as nothing at all (a fall is all flowing blocks),
	 * and the ground under it left open - reported as "there are gaps between the stepped flowing lava
	 * in a lava fall, the flowing state is wrong on the Rust side". It was.
	 *
	 * The bit is the other half of the same thing. The mesher draws the two halves with one set of
	 * sprites - they are one liquid to look at - but it asks the game's question wherever the game does:
	 * whether the same *object* is above a block (which decides the surface height), whether a neighbour
	 * affects the flow, and what a corner averages. Getting *that* wrong is what "the lava's flowing
	 * state still does not match the game" was, because a source beside a flowing block is two liquids
	 * that do not join.
	 *
	 * The bit used to be documented as "falling", which nothing ever wrote: a falling fluid is not a
	 * different fluid - its `FALLING` property is on the state and its type is the flowing object, like
	 * any spreading block.
	 */
	private fun fluidByte(state: BlockState?): Byte {
		if (state == null) return 0

		val fluid = state.fluidState
		if (fluid.isEmpty) return 0

		val type = fluid.type

		val (kind, flowing) = when {
			type.`isSame`(Fluids.WATER) -> 1 to false
			type.`isSame`(Fluids.FLOWING_WATER) -> 1 to true
			type.`isSame`(Fluids.LAVA) -> 2 to false
			type.`isSame`(Fluids.FLOWING_LAVA) -> 2 to true
			else -> 3 to false
		}

		return (kind or (fluid.amount shl 2) or (if (flowing) 0b0100_0000 else 0)).toByte()
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
