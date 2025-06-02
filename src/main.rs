use deno_ast::MediaType;
// use deno_ast::ParseParams;
use deno_core::error::CoreError;
use deno_core::error::ModuleLoaderError;
use deno_core::extension;
use deno_core::op2;
use deno_core::ModuleLoadResponse;
use deno_core::ModuleSourceCode;
use deno_error::JsErrorBox;
use std::env;
use std::rc::Rc;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use deno_ast::ParseParams;

// Global chroot configuration
static CHROOT_CONFIG: OnceLock<ChrootConfig> = OnceLock::new();

#[derive(Debug)]
struct ChrootConfig {
    root_path: PathBuf,
}

impl ChrootConfig {
    fn new(root_path: PathBuf) -> Self {
        Self { root_path }
    }

    fn validate_path(&self, path: &str) -> Result<PathBuf, std::io::Error> {
        let path = Path::new(path);
        let normalized = self.root_path.join(path).canonicalize()?;
        
        if !normalized.starts_with(&self.root_path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Path escapes chroot directory",
            ));
        }
        
        Ok(normalized)
    }
}

fn init_chroot(root_path: &str) -> Result<(), std::io::Error> {
    let root_path = Path::new(root_path).canonicalize()?;
    CHROOT_CONFIG.set(ChrootConfig::new(root_path)).unwrap();
    Ok(())
}

#[op2(async)]
#[string]
async fn op_read_file(
  #[string] path: String,
) -> Result<String, std::io::Error> {
  let config = CHROOT_CONFIG.get().ok_or_else(|| {
    std::io::Error::new(
      std::io::ErrorKind::NotFound,
      "Chroot not initialized",
    )
  })?;
  
  let validated_path = config.validate_path(&path)?;
  tokio::fs::read_to_string(validated_path).await
}

#[op2(async)]
async fn op_write_file(
  #[string] path: String,
  #[string] contents: String,
) -> Result<(), std::io::Error> {
  let config = CHROOT_CONFIG.get().ok_or_else(|| {
    std::io::Error::new(
      std::io::ErrorKind::NotFound,
      "Chroot not initialized",
    )
  })?;
  
  let validated_path = config.validate_path(&path)?;
  tokio::fs::write(validated_path, contents).await
}

#[op2(fast)]
fn op_remove_file(#[string] path: String) -> Result<(), std::io::Error> {
  std::fs::remove_file(path)
}

#[op2(fast)]
fn op_process_task(#[string] path: String) -> Result<(), std::io::Error> {
  std::fs::remove_file(path)
}

#[op2(async)]
#[string]
async fn op_fetch(#[string] url: String) -> Result<String, JsErrorBox> {
  reqwest::get(url)
    .await
    .map_err(|e| JsErrorBox::type_error(e.to_string()))?
    .text()
    .await
    .map_err(|e| JsErrorBox::type_error(e.to_string()))
}

#[op2(async)]
async fn op_set_timeout(delay: f64) {
  tokio::time::sleep(std::time::Duration::from_millis(delay as u64)).await;
}

struct TsModuleLoader;

impl deno_core::ModuleLoader for TsModuleLoader {
  fn resolve(
    &self,
    specifier: &str,
    referrer: &str,
    _kind: deno_core::ResolutionKind,
  ) -> Result<deno_core::ModuleSpecifier, ModuleLoaderError> {
    deno_core::resolve_import(specifier, referrer).map_err(Into::into)
  }

  fn load(
    &self,
    module_specifier: &deno_core::ModuleSpecifier,
    _maybe_referrer: Option<&reqwest::Url>,
    _is_dyn_import: bool,
    _requested_module_type: deno_core::RequestedModuleType,
  ) -> ModuleLoadResponse {
    let module_specifier = module_specifier.clone();

    let module_load = move || {
      let path = module_specifier.to_file_path().unwrap();
      let media_type = MediaType::from_path(&path);

      let (module_type, should_transpile) = match MediaType::from_path(&path) {
        MediaType::JavaScript | MediaType::Mjs | MediaType::Cjs => {
          (deno_core::ModuleType::JavaScript, false)
        }
        MediaType::Jsx => (deno_core::ModuleType::JavaScript, true),
        MediaType::TypeScript
        | MediaType::Mts
        | MediaType::Cts
        | MediaType::Dts
        | MediaType::Dmts
        | MediaType::Dcts
        | MediaType::Tsx => (deno_core::ModuleType::JavaScript, true),
        MediaType::Json => (deno_core::ModuleType::Json, false),
        _ => panic!("Unknown extension {:?}", path.extension()),
      };

      let code = std::fs::read_to_string(&path)?;

      let code = if should_transpile {
        let parsed = deno_ast::parse_module(ParseParams {
          specifier: module_specifier.clone(),
          text: code.into(),
          media_type,
          capture_tokens: false,
          scope_analysis: false,
          maybe_syntax: None,
        })
        .map_err(JsErrorBox::from_err)?;
        parsed
          .transpile(
            &Default::default(),
            &Default::default(),
            &Default::default(),
          )
          .map_err(JsErrorBox::from_err)?
          .into_source()
          .text
      } else {
        code
      };

      let module = deno_core::ModuleSource::new(
        module_type,
        ModuleSourceCode::String(code.into()),
        &module_specifier,
        None,
      );
      Ok(module)
    };

    ModuleLoadResponse::Sync(module_load())
  }
}

// static RUNTIME_SNAPSHOT: &[u8] =
//   include_bytes!(concat!("/Users/robertwendt/runjs", "/RUNJS_SNAPSHOT.bin"));

extension!(
  runjs,
  ops = [
    op_read_file,
    op_write_file,
    op_remove_file,
    op_fetch,
    op_set_timeout,
  ],
  esm_entry_point = "ext:runjs/runtime.js",
  esm = [dir "src", "runtime.js"],
);

async fn run_js(file_path: &str) -> Result<(), CoreError> {
  let main_module =
    deno_core::resolve_path(file_path, env::current_dir()?.as_path())
      .map_err(JsErrorBox::from_err)?;
  let mut js_runtime = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
    module_loader: Some(Rc::new(TsModuleLoader)),

    // startup_snapshot: Some(RUNTIME_SNAPSHOT),
    extensions: vec![runjs::init()],

    ..Default::default()
  });

  let mod_id = js_runtime.load_main_es_module(&main_module).await?;
  let result = js_runtime.mod_evaluate(mod_id);
  js_runtime.run_event_loop(Default::default()).await?;
  result.await
}

fn main() {
  let args = &env::args().collect::<Vec<String>>()[1..];

  if args.is_empty() {
    eprintln!("Usage: runjs <file>");
    std::process::exit(1);
  }

  // Initialize chroot to current directory
  if let Err(error) = init_chroot(".") {
    eprintln!("Failed to initialize chroot: {error}");
    std::process::exit(1);
  }

  let file_path = &args[0];

  let runtime = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .unwrap();
  if let Err(error) = runtime.block_on(run_js(file_path)) {
    eprintln!("error: {error}");
  }
}