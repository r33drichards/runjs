use runjs::RunJs;
use std::path::PathBuf;
use std::{env, result};

#[tokio::main]
async fn main() {
    let args = &env::args().collect::<Vec<String>>()[1..];

    if args.is_empty() {
        eprintln!("Usage: runjs <file>");
        std::process::exit(1);
    }

    let result = RunJs::new(runjs::RunJsConfig {
        chroot_path: Some(PathBuf::from(".")),
    })
    .run_string("console.log('hello rust')")
    .await;
}
