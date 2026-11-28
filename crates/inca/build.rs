//! Embeds the built-in Content Browser examples (assets/examples/**/*.mel, each with an
//! optional same-named .png thumbnail) so they ship inside the single executable.

use std::path::{Path, PathBuf};

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|e| e == "mel") {
            out.push(p);
        }
    }
}

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("assets").join("examples");
    println!("cargo:rerun-if-changed=assets/examples");
    let mut files = vec![];
    walk(&root, &mut files);
    let mut src = String::from("/// (path relative to the examples root, MEL source, PNG thumbnail)\npub static EXAMPLE_FILES: &[(&str, &str, Option<&[u8]>)] = &[\n");
    for f in files {
        let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        let png = f.with_extension("png");
        let thumb = if png.exists() { format!("Some(include_bytes!({:?}))", png.to_string_lossy()) } else { "None".to_string() };
        src += &format!("    ({:?}, include_str!({:?}), {}),\n", rel, f.to_string_lossy(), thumb);
    }
    src += "];\n";
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("examples_gen.rs");
    std::fs::write(out, src).unwrap();
}
