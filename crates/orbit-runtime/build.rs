//! Generates the gRPC binding of the worker protocol from `proto/worker.proto`
//! (a copy of `protocol/orbit/worker/v1/worker.proto`, kept identical by a
//! test). `protox` compiles the schema in pure Rust, so no `protoc` is needed.

fn main() {
    println!("cargo:rerun-if-changed=proto/worker.proto");
    let fds = protox::compile(["worker.proto"], ["proto"]).expect("worker.proto compiles");
    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_fds(fds)
        .expect("generate gRPC code");
}
