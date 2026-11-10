fn main() {
    println!("cargo:rerun-if-changed=protocol/collection.proto");
    println!("cargo:rerun-if-changed=protocol/soloist_session.proto");
    protobuf_codegen::Codegen::new()
        .pure()
        .cargo_out_dir("spotify-protocol")
        .include("protocol")
        .input("protocol/collection.proto")
        .input("protocol/soloist_session.proto")
        .run_from_script();
}
