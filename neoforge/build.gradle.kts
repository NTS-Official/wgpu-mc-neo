import org.gradle.api.publish.maven.MavenPublication
import org.gradle.api.tasks.Copy
import org.gradle.api.tasks.Delete
import org.gradle.api.tasks.compile.JavaCompile
import org.gradle.jvm.tasks.Jar
import org.gradle.language.jvm.tasks.ProcessResources
import org.gradle.process.CommandLineArgumentProvider
import org.gradle.process.JavaForkOptions

plugins {
	`java-library`
	`maven-publish`
	kotlin("jvm") version "2.3.20"
	id("net.neoforged.gradle.userdev") version "7.1.21"
}

val neoforgeMinecraftVersion = providers.gradleProperty("neoforge_minecraft_version").get()
val neoforgeNeoVersion = providers.gradleProperty("neoforge_neo_version").get()
val neoforgeModVersion = providers.gradleProperty("neoforge_mod_version").get()
val neoforgeMavenGroup = providers.gradleProperty("neoforge_maven_group").get()
val neoforgeKotlinVersion = providers.gradleProperty("neoforge_kotlin_version").getOrElse("2.3.20")

val modId = "wgpu_mc"
val modName = "Neolectrum"
val modLicense = "MPLv2"
val loaderVersionRange = "[3,)"
// Minecraft 26.1 is the new stable modding baseline; 26.2 is the next feature release.
val minecraftVersionRange = "[$neoforgeMinecraftVersion,$neoforgeMinecraftVersion.999)"

group = neoforgeMavenGroup
version = neoforgeModVersion

base {
	archivesName.set(modId)
}

sourceSets.main {
	resources {
		srcDir("src/generated/resources")
		exclude("**/*.bbmodel")
		exclude("**/.cache")
		exclude("assets/electrum/**")
	}
}

java {
	toolchain {
		// Minecraft 26.1 moved to Java 25.
		languageVersion.set(JavaLanguageVersion.of(25))
	}
	withSourcesJar()
}

kotlin {
	jvmToolchain(25)
}

// ---------------------------------------------------------------------------
// The Kotlin runtime has to be inside the jar.
//
// This module is written in Kotlin, and nothing else on a player's classpath brings the runtime:
// FML and NeoForge are Java, and Minecraft does not ship Kotlin either. A development run does not
// show the omission, because the Kotlin Gradle plugin puts the stdlib on the *run* classpath - which
// is why it survived until the first jar was started in a real instance. There the game dies the
// moment a Kotlin class is loaded, and the first one it loads is the mixin that runs on `Main.main`:
//
//   java.lang.NoClassDefFoundError: kotlin/jvm/internal/Intrinsics
//
// `jarJar` embeds the stdlib under `META-INF/jarjar/` next to the metadata FML's jar-in-jar loader
// reads, so the published `<name>-all.jar` carries its own runtime. KotlinForForge ships one as
// well; two copies is the ordinary jar-in-jar situation and the versions are compatible, so an
// instance that has it keeps working - an instance that does not is the case this fixes.
// ---------------------------------------------------------------------------
dependencies {
	constraints {
		implementation("org.jetbrains.kotlin:kotlin-stdlib:$neoforgeKotlinVersion") {
			because("The Kotlin plugin adds the stdlib at its own version; pin it to the one jarJar embeds.")
		}
	}

	// Adding a dependency to `jarJar` is what enables the `jarJar` task; its output (classifier
	// `all`) is the artifact `assemble` and `publish` hand out.
	add("jarJar", "org.jetbrains.kotlin:kotlin-stdlib:$neoforgeKotlinVersion")
}

val localRuntime = configurations.named("localRuntime")
configurations.named("runtimeClasspath") {
	extendsFrom(localRuntime.get())
}

dependencies {
	implementation("net.neoforged:neoforge:$neoforgeNeoVersion")
}

repositories {
	maven {
		name = "NeoForged"
		setUrl("https://maven.neoforged.net/releases")
	}
	mavenCentral()
}

// ---------------------------------------------------------------------------
// Native library packaging
//
// The previous (1.21.1) revision pulled the JNI bridge in through the
// `fr.stardustenterprises.rust.importer` plugin, which is not available for the
// NeoForge 26.1 toolchain. The native library is now picked straight out of the
// Rust workspace output, and the task degrades to a no-op until it has been
// built with `cargo build --release`.
// ---------------------------------------------------------------------------
val rustProjectDir = rootProject.layout.projectDirectory.dir("rust")
val rustReleaseDir = rustProjectDir.dir("target/release")
val nativeLibraryFileName = System.mapLibraryName("wgpu_mc_jni")
val nativeLibrary = rustReleaseDir.file(nativeLibraryFileName)

// The PDB the release build writes beside the library, when the profile asks for one (`debug =
// "line-tables-only"` in `rust/Cargo.toml`). It rides along into this module's resources, and into
// the published jar with them, for one reason: the game unpacks both into the launcher's natives
// directory, and PIX resolves a timing capture's function names through the PDB that sits *there* - a
// debugger looks for symbols beside the module, not inside the mod's jar. Absent when the Rust build
// produced none, which is not an error.
val nativeSymbols = rustReleaseDir.file(nativeLibraryFileName.substringBeforeLast('.') + ".pdb")

val copyNatives = tasks.register<Copy>("copyNatives") {
	description = "Copies the freshly built Rust JNI bridge into this module's resources."
	group = "build"
	onlyIf { nativeLibrary.asFile.exists() }
	from(nativeLibrary) {
		into("assets/$modId/natives")
	}
	from(nativeSymbols) {
		into("assets/$modId/natives")
	}
	into(layout.buildDirectory.dir("generated/natives"))
}

sourceSets.main {
	resources.srcDir(copyNatives.map { it.destinationDir })
}

val unpackExports = tasks.register<Copy>("unpackExports") {
	onlyIf { layout.buildDirectory.file("resources/main/export.zip").get().asFile.exists() }
	from(zipTree(layout.buildDirectory.file("resources/main/export.zip")))
	into(layout.buildDirectory.dir("resources/main"))
	finalizedBy("deleteExports")
}

tasks.register<Delete>("deleteExports") {
	description = "Deletes the exported files after they have been processed."
	delete(layout.buildDirectory.file("resources/main/export.zip"))
}

tasks.named<ProcessResources>("processResources") {
	dependsOn(copyNatives)
	finalizedBy(unpackExports, "deleteExports")

	val replaceProperties = mapOf(
		"minecraft_version" to neoforgeMinecraftVersion,
		"minecraft_version_range" to minecraftVersionRange,
		"neo_version" to neoforgeNeoVersion,
		"loader_version_range" to loaderVersionRange,
		"mod_id" to modId,
		"mod_name" to modName,
		"mod_license" to modLicense,
		"mod_version" to neoforgeModVersion
	)
	inputs.properties(replaceProperties)

	filesMatching("META-INF/neoforge.mods.toml") {
		expand(replaceProperties)
	}
}

tasks.named<Jar>("jar") {
	dependsOn(unpackExports, "deleteExports")

	// The PDB stays in the jar. It is ~47 MB of line tables uncompressed, and it is the only way a
	// capture taken on a machine that runs the published jar can show function names - which is the
	// machine a report about a slow frame comes from. `jarJar` copies this task's output into the
	// published jar, so the symbols travel with the library they match.
}

listOf("runClient", "runData", "runGameTestServer", "runServer").forEach { runTaskName ->
	tasks.matching { it.name == runTaskName }.configureEach {
		dependsOn(unpackExports)
	}
}

// ---------------------------------------------------------------------------
// NeoForge's early loading window, turned off for development runs.
//
// It draws its splash screen through OpenGL from Minecraft's render thread, which this mod cannot
// survive without an abort - a production instance hits exactly that, and
// `dev.birb.wgpu.backend.EarlyWindow` now takes the screen out of the way there (the README has the
// stack and the reasoning). This task is the cruder, older answer to the same problem: it writes
// FML's own switch, which is read before any mod is loaded, so a dev run never creates the screen at
// all and opens straight into the game window.
//
// The cost is that the dev loop does not exercise `EarlyWindow`, so this is deliberately only a
// convenience - deleting `earlyWindowControl` from runs/client/config/fml.toml makes runClient take
// the same path a player's instance does. The task edits that one key and leaves the rest of the
// file to FML, which fills in everything it does not find.
// ---------------------------------------------------------------------------
val configureEarlyWindow = tasks.register("configureEarlyWindow") {
	description = "Disables NeoForge's early loading window, which needs a GL context this mod never creates."
	group = "neoforge"

	val configFile = layout.projectDirectory.file("runs/client/config/fml.toml").asFile
	outputs.file(configFile)

	doLast {
		val key = "earlyWindowControl"
		val wanted = "$key = false"

		if (!configFile.exists()) {
			// The rest of the file is FML's to write: its config spec fills in every key it does
			// not find, so seeding just this one is enough.
			configFile.parentFile.mkdirs()
			configFile.writeText("#Written by the wgpu-mc build; see configureEarlyWindow in build.gradle.kts\n$wanted\n")
			logger.lifecycle("Seeded ${configFile.path} with $wanted")
			return@doLast
		}

		val lines = configFile.readLines()
		val rewritten = lines.map { line ->
			if (line.trimStart().startsWith("$key ") || line.trimStart().startsWith("$key=")) wanted else line
		}

		if (rewritten != lines) {
			configFile.writeText(rewritten.joinToString("\n", postfix = "\n"))
			logger.lifecycle("Set $wanted in ${configFile.path}")
		}
	}
}

tasks.matching { it.name == "runClient" }.configureEach {
	dependsOn(configureEarlyWindow)
}

// The backend binds the Rust C ABI through java.lang.foreign, which is a restricted method on
// JDK 24+. Without this flag a dev run logs a warning per downcall; a production launcher is
// expected to pass the same flag, and the mod still works if it does not. Both names are needed:
// the mod's own classes are loaded as the named module `wgpu_mc`, while the FFM calls that reach
// the JDK from the unnamed module (the class path) are covered by ALL-UNNAMED.
//
// The guard is the *interface*, not `JavaExec`: what a run task has to be here is something that
// forks a JVM and takes arguments for it, which is what `jvmArgs` is a method of. NeoGradle's run
// tasks happen to *be* `JavaExec` today - `help --task runClient` says so - but that is a property
// of the plugin, not of the flag: a run task from another plugin, or a decorated one, implements
// `JavaForkOptions` without extending `JavaExec`, and a guard that named the class would silently
// stop applying to it.
//
// The flag goes on as a *provider*, not through `jvmArgs`, and that is not a style choice: NeoGradle
// configures the run tasks from its own run config after this block has run, and it *replaces* their
// argument list. Checked with an init script that read the task back - `runClient.jvmArgs` came out
// as the plugin's own `[--sun-misc-unsafe-memory-access=allow, --enable-native-access=ALL-UNNAMED,
// --add-opens, ...]` with this flag nowhere in it, at `projectsEvaluated` and again at
// `taskGraph.whenReady`. So the dev loop has been running *without* the module half of the flag,
// which is the half the mod's own classes need - they are loaded as the named module `wgpu_mc`, and
// `--enable-native-access=ALL-UNNAMED` covers the class path, not them. The symptom is a warning per
// downcall and nothing else, which is why it went unnoticed.
//
// A provider is a second list that the command line is built from, and a later `setJvmArgs` cannot
// drop it.
tasks.matching { it.name.startsWith("run") }.configureEach {
	if (this is JavaForkOptions) {
		jvmArgumentProviders.add(CommandLineArgumentProvider {
			listOf("--enable-native-access=ALL-UNNAMED,wgpu_mc")
		})
	}
}

// Diagnostics - the frame/trace dumps behind `dev.birb.wgpu.backend.Diagnostics` - are switched on
// per run instead of from here, because the launcher's JVM arguments turned out to be one more
// thing that can silently not arrive:
//
//   gradlew :wgpu-mc-neoforge:runClient -Dwgpu_mc.diagnostics=true     # or
//   New-Item neoforge/runs/client/wgpu-dump-frames                     # no launcher support needed
//
// The marker file is the one that works everywhere, so it is the documented one.

tasks.withType<Jar>().configureEach {
	from(rootProject.file("LICENSE")) {
		rename { "${it}_${base.archivesName.get()}" }
	}
}

tasks.withType<JavaCompile>().configureEach {
	options.encoding = "UTF-8"
	options.release.set(25)
}

publishing {
	publications {
		register<MavenPublication>("mavenJava") {
			from(components["java"])
		}
	}
	repositories {
		maven {
			url = uri(layout.projectDirectory.dir("repo"))
		}
	}
}
