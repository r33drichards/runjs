use runjs::RunJs;
use std::env;
use std::path::PathBuf;

fn main() {
    let args = &env::args().collect::<Vec<String>>()[1..];

    if args.is_empty() {
        eprintln!("Usage: runjs <file>");
        std::process::exit(1);
    }

    let config = runjs::RunJsConfig {
        chroot_path: Some(PathBuf::from(".")),
    };
    let mut runjs = RunJs::new(config);

    let file_path = &args[0];

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    if let Err(error) = runtime.block_on(runjs.run_file(file_path)) {
        eprintln!("error: {error}");
    }
}