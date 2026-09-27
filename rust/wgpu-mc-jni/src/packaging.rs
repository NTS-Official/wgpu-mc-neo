//! The mod's own metadata: the version numbers NeoForge is told, in the two files that carry them.
//!
//! `neoforge/updates.json` is what NeoForge's update checker fetches (the `updateJSONURL` in
//! `neoforge.mods.toml`) so that the mod list can show a newer build. **Nothing in this tree reads it**: a
//! version bump that forgets it leaves a file that still parses, and whose `promos` answer "you are up to
//! date" to every older build - which looks exactly like a project that has no updates at all.
//!
//! The format is NeoForge's (`docs.neoforged.net/docs/misc/updatechecker`), and the key it looks up is not
//! a guess: `VersionChecker#process` asks `FMLLoader.getCurrent().getVersionInfo().mcVersion()` for
//! `"<mcversion>-latest"` and `"<mcversion>-recommended"`, and that version is the **`minecraft` mod file's
//! own version** - which the client logs as
//!
//! ```text
//! Found valid mod file neoforge-26.1.2.109.jar with {minecraft} mods - versions {26.1.2}
//!         Minecraft 26.1.2 (minecraft)
//! ```
//!
//! so the key is the Minecraft version, hotfix and all (`26.1.2`, not `26.1`). The changelog is read from
//! `json[mcVersion]`, and only the entries *newer* than the running build are shown.
//!
//! `wgpu_mc_jni::abi_tests` is the other test of this kind: the two bridges of the JNI, and this is the two
//! files that have to name the same version.

#[cfg(test)]
mod tests {
    use serde_json::Value;

    /// The file that owns the numbers, and the files that consume them.
    const GRADLE: &str = include_str!("../../../gradle.properties");
    const UPDATES: &str = include_str!("../../../neoforge/updates.json");
    const MODS_TOML: &str =
        include_str!("../../../neoforge/src/main/resources/META-INF/neoforge.mods.toml");

    fn property(name: &str) -> String {
        let prefix = format!("{name}=");

        GRADLE
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("`{name}` is not in gradle.properties"))
            .trim()
            .to_string()
    }

    fn updates() -> Value {
        serde_json::from_str(UPDATES)
            .expect("updates.json is valid JSON - NeoForge will not read it if it is not")
    }

    /// The version the mod file is stamped with - the `version` of the `minecraft` mod file, which is what
    /// the checker keys `promos` on. NeoForge 26.1 versions are `<minecraft>-<hotfix>.<release>` (see the
    /// note in `gradle.properties`), so the Minecraft version is that string without its last part.
    fn minecraft_version() -> String {
        let neo = property("neoforge_neo_version");

        let mut parts = neo.split('.').collect::<Vec<_>>();

        assert!(
            parts.len() == 4,
            "the NeoForge version is four parts, `<minecraft>-<hotfix>.<release>`: {neo}"
        );

        parts.pop();

        parts.join(".")
    }

    /// The build this tree produces is the one the update file offers, for the Minecraft version the game
    /// will ask about.
    #[test]
    fn the_update_file_offers_the_version_this_tree_builds() {
        let updates = updates();
        let version = property("neoforge_mod_version");
        let minecraft = minecraft_version();

        assert!(
            updates["homepage"]
                .as_str()
                .is_some_and(|url| !url.is_empty()),
            "`homepage` is the link the player is shown for an outdated build, and it is missing"
        );

        let promos = updates["promos"]
            .as_object()
            .expect("`promos` is an object of `<mcversion>-<channel>` to a version");

        let key = format!("{minecraft}-latest");

        assert_eq!(
            promos.get(&key).and_then(Value::as_str),
            Some(version.as_str()),
            "the game looks up `{key}`, so that is the entry that has to name the version this tree \
             builds ({version}). `promos` says: {promos:?}"
        );

        // And the changelog for it, under the same Minecraft version: the checker reads `json[mcVersion]`
        // and shows the entries newer than the running build.
        assert!(
            updates[minecraft.as_str()][version.as_str()].is_string(),
            "`{minecraft}` has no changelog entry for `{version}`, so an outdated player is told there is \
             an update and shown nothing about it"
        );
    }

    /// Every channel a promo names has to be a Minecraft version the file also lists, or the checker finds
    /// a version and no changelog for it.
    #[test]
    fn every_promo_names_a_listed_minecraft_version() {
        let updates = updates();
        let promos = updates["promos"]
            .as_object()
            .expect("`promos` is an object");

        for (key, version) in promos {
            let minecraft = key
                .rsplit_once('-')
                .map(|(minecraft, _channel)| minecraft)
                .unwrap_or_else(|| panic!("`{key}` is not `<mcversion>-<channel>`"));

            let version = version.as_str().expect("a promo names a version");

            assert!(
                updates[minecraft][version].is_string(),
                "`{key}` names {version}, and `{minecraft}` has no entry for it: the checker would flash \
                 an update with no changelog"
            );
        }
    }

    /// The URL the mod file points the checker at is this file, on the branch the project develops on.
    #[test]
    fn the_mod_file_points_at_this_file() {
        let url = MODS_TOML
            .lines()
            .find_map(|line| line.strip_prefix("updateJSONURL="))
            .expect("`updateJSONURL` is what makes NeoForge check at all");

        assert!(
            url.contains("/master/neoforge/updates.json"),
            "the update URL has to name this file on the development branch, or the checker reads \
             something else: {url}"
        );
    }
}
