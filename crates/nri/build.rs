use std::env;
use std::fs;
use std::path::PathBuf;
use ttrpc_codegen::Customize;

fn main() {
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let proto_file = "src/api.proto";

    // Tell cargo to rerun this if the proto file changes
    println!("cargo:rerun-if-changed={}", proto_file);

    // Generate the ttrpc code
    ttrpc_codegen::Codegen::new()
        .out_dir(&out_dir)
        .inputs([proto_file])
        .include("src")
        .rust_protobuf() // generate both protobuf messages and ttrpc services
        .customize(Customize {
            async_client: true, // Generate async code for client
            async_server: true, // Generate async code for server
            ..Default::default()
        })
        .run()
        .expect("Failed to generate ttrpc bindings");

    // Fix the generated file by replacing inner attributes with outer attributes
    let api_ttrpc_path = out_dir.join("api_ttrpc.rs");
    if let Ok(content) = fs::read_to_string(&api_ttrpc_path) {
        let fixed_content = content
            .replace("#![cfg_attr(", "#[cfg_attr(")
            .replace("#![allow(", "#[allow(")
            .replace("#[allow(box_pointers)]\n", "");
        fs::write(api_ttrpc_path, fixed_content).expect("Failed to write fixed api_ttrpc.rs");
    }
}
