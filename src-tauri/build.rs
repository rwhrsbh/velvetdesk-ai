fn main() {
    // The licence key is compiled in with `option_env!`, and cargo has no way
    // of knowing that on its own: without this line a rebuild after setting
    // the variable would quietly reuse the object file built without it, and
    // the release would ship as a free build.
    println!("cargo:rerun-if-env-changed=VD_LICENSE_PUBLIC_KEY");
    tauri_build::build()
}
