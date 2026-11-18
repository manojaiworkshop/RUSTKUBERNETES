//! Embeds the built web UI (`web/out`, produced by `npm run build` in `admin/web`) into the
//! binary as `ASSETS: &[(&str, &[u8])]`, like pgrustadmin. Without a build the table is empty
//! and the server answers `/` with a short hint instead of the console.

use std::fmt::Write as _;
use std::path::Path;

fn walk(dir: &Path, root: &Path, out: &mut String) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            walk(&p, root, out);
        } else {
            let rel = p.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            let _ = writeln!(out, "    ({:?}, include_bytes!({:?}) as &[u8]),", format!("/{rel}"), p.to_string_lossy());
        }
    }
}

fn main() {
    let root = Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("web/out");
    println!("cargo:rerun-if-changed={}", root.display());
    println!("cargo:rerun-if-changed=assets");
    let mut out = String::from("pub static ASSETS: &[(&str, &[u8])] = &[\n");
    walk(&root, &root, &mut out);
    out += "];\n";
    std::fs::write(Path::new(&std::env::var("OUT_DIR").unwrap()).join("assets.rs"), out).unwrap();
}
