//! `xorgate-rs -v` / `--version` — `cli.tsx:24-26,64-67` over
//! `formatBuildInfo` (`src/lib/build-info.ts:51`).
//!
//! The TS prints `xorgate <packageVersion> (<version>, <channel>)`; this binary
//! prints its own name and its own build (`xorgate-rs 0.0.1 (abc1234, dev)`)
//! so a field report names the agent that is actually installed (T8/D20:
//! build identity differs by design).

/// The program name in the banner and `-v`.
pub const PROGRAM: &str = "xorgate-rs";

/// `xorgate-rs <formatBuildInfo()>`.
pub fn line() -> String {
    format!("{PROGRAM} {}", crate::build_info::load().format())
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_the_binary_and_its_build() {
        let l = super::line();
        assert!(l.starts_with("xorgate-rs "), "{l}");
        assert!(l.ends_with(')'), "{l}");
    }
}
