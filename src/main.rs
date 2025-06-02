use clap::Parser;
use runjs::RunJs;
use std::path::PathBuf;

/// A JavaScript/TypeScript runtime with chroot capabilities
#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// The JavaScript/TypeScript file to run
    #[arg(required = true)]
    file: PathBuf,

    /// Optional chroot path (defaults to current directory)
    #[arg(long, short)]
    chroot: Option<PathBuf>,
}


#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let runjs = RunJs::new(runjs::RunJsConfig {
        chroot_path: cli.chroot.or_else(|| Some(PathBuf::from("."))),
    });

    

}
