// SPDX-License-Identifier: EUPL-1.2
//! Embeds the emoji images and picker table, and compiles the application
//! icon (assets/nuschl.rc) into nuschl.exe with the target's resource
//! compiler, as GPUI's own build script already requires.
//!
//! Emoji: `NUSCHL_EMOJI_ASSETS` names the output of nix/emoji.nix (every Nix
//! build sets it); without it, `emoji-assets` in the workspace root is used
//! (`just emoji-assets`, and the Debian source package ships it). The build
//! fails without either: a desktop without its emoji would be a silent
//! downgrade.
use std::{env, fs, path::PathBuf};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    emoji()?;
    println!("cargo:rerun-if-changed=assets/nuschl.rc");
    println!("cargo:rerun-if-changed=assets/nuschl.ico");
    embed_resource::compile("assets/nuschl.rc", embed_resource::NONE).manifest_required()?;
    Ok(())
}

/// Writes `emoji_assets.rs` into OUT_DIR: the picker table and every image,
/// sorted by key, each with whether it is a text-default character.
fn emoji() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-env-changed=NUSCHL_EMOJI_ASSETS");
    let root = match env::var_os("NUSCHL_EMOJI_ASSETS") {
        Some(path) => PathBuf::from(path),
        None => PathBuf::from(env::var("CARGO_MANIFEST_DIR")?).join("../../emoji-assets"),
    };
    let root = fs::canonicalize(&root).map_err(|error| {
        format!(
            "emoji assets not found at {} ({error}): run `just emoji-assets`, \
             or set NUSCHL_EMOJI_ASSETS to the output of `nix build .#emojiAssets`",
            root.display()
        )
    })?;
    let (table, images) = (root.join("emoji.txt"), root.join("images.txt"));
    println!("cargo:rerun-if-changed={}", table.display());
    println!("cargo:rerun-if-changed={}", images.display());
    let mut entries = Vec::new();
    for line in fs::read_to_string(&images)?.lines() {
        let (key, text) = match line.split_once('\t') {
            Some((key, "text")) => (key, true),
            None => (line, false),
            Some(_) => return Err(format!("unexpected line in images.txt: {line}").into()),
        };
        let valid = !key.is_empty()
            && key.split('-').all(|part| {
                !part.is_empty() && part.len() <= 6 && part.bytes().all(|b| b.is_ascii_hexdigit())
            });
        if !valid {
            return Err(format!("unexpected image key {key:?}").into());
        }
        // Gzip-compressed (nix/emoji.nix); GPUI's SVG parser reads it as is.
        let svg = root.join("svgz").join(format!("{key}.svgz"));
        if !svg.is_file() {
            return Err(format!("{} is missing", svg.display()).into());
        }
        entries.push((key.to_string(), text, svg));
    }
    entries.sort();
    entries.dedup_by(|a, b| a.0 == b.0);
    let mut out = String::from(
        "/// The picker's table (nix/emoji.py).\n\
         pub const TABLE: &str = include_str!(",
    );
    out += &format!("{:?});\n", table.display().to_string());
    out += "/// Every Twemoji image by key, sorted: (key, shown as text by default,\n";
    out += "/// gzip-compressed SVG).\n";
    out += "pub static IMAGES: &[(&str, bool, &[u8])] = &[\n";
    for (key, text, svg) in &entries {
        out += &format!(
            "    ({key:?}, {text}, include_bytes!({:?})),\n",
            svg.display().to_string()
        );
    }
    out += "];\n";
    fs::write(
        PathBuf::from(env::var("OUT_DIR")?).join("emoji_assets.rs"),
        out,
    )?;
    Ok(())
}
