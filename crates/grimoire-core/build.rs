fn main() {
    let manifest_dir = std::path::PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("Cargo should set CARGO_MANIFEST_DIR"),
    );
    let proto_dir = manifest_dir.join("proto");
    let schema = proto_dir.join("grimoire.proto");
    prost_build::compile_protos(&[schema], &[proto_dir]).expect("Grimoire protocol should compile");
}
