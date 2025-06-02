use deno_ast::MediaType;
use deno_core::error::CoreError;
use deno_core::error::ModuleLoaderError;
use deno_core::extension;
use deno_core::op2;
use deno_core::ModuleLoadResponse;
use deno_core::ModuleSourceCode;
use deno_error::JsErrorBox;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use deno_ast::ParseParams;

/// Configuration for the RunJS runtime
#[derive(Debug, Clone)]
pub struct RunJsConfig {
    /// The root path for chroot operations. If None, chroot is disabled.
    pub chroot_path: Option<PathBuf>,
}

impl Default for RunJsConfig {
    fn default() -> Self {
        Self {
            chroot_path: None,
        }
    }
}

/// The main RunJS runtime instance
pub struct RunJs {
    config: RunJsConfig,
}

impl RunJs {
    /// Create a new RunJS instance with the given configuration
    pub fn new(config: RunJsConfig) -> Self {
        Self { config }
    }

    /// Create a new RunJS instance with default configuration (no chroot)
    pub fn new_default() -> Self {
        Self::new(RunJsConfig::default())
    }

    /// Run a JavaScript/TypeScript file
    pub async fn run_file(&self, file_path: &str) -> Result<(), CoreError> {
        let main_module = deno_core::resolve_path(file_path, std::env::current_dir()?.as_path())
            .map_err(JsErrorBox::from_err)?;

        // Set up chroot if configured
        if let Some(chroot_path) = &self.config.chroot_path {
            let chroot_path = chroot_path.canonicalize().map_err(|e| {
                CoreError::from(JsErrorBox::type_error(format!(
                    "Failed to canonicalize chroot path: {}",
                    e
                )))
            })?;
            
            // Set new chroot config, replacing any existing one
            if CHROOT_CONFIG.set(ChrootConfig::new(chroot_path)).is_err() {
                return Err(CoreError::from(JsErrorBox::type_error(
                    "Failed to set chroot configuration"
                )));
            }
        }

        let mut js_runtime = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
            module_loader: Some(Rc::new(TsModuleLoader)),
            extensions: vec![runjs::init()],
            ..Default::default()
        });

        let mod_id = js_runtime.load_main_es_module(&main_module).await?;
        let result = js_runtime.mod_evaluate(mod_id);
        js_runtime.run_event_loop(Default::default()).await?;
        result.await
    }
}

// Global chroot configuration
static CHROOT_CONFIG: std::sync::OnceLock<ChrootConfig> = std::sync::OnceLock::new();

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
    
    // Ensure parent directory exists
    if let Some(parent) = validated_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    
    tokio::fs::write(validated_path, contents).await
}

#[op2(fast)]
fn op_remove_file(#[string] path: String) -> Result<(), std::io::Error> {
    let config = CHROOT_CONFIG.get().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Chroot not initialized",
        )
    })?;
    
    let validated_path = config.validate_path(&path)?;
    std::fs::remove_file(validated_path)
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

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use std::fs;
    use tempfile::TempDir;

    async fn setup_test_env() -> Result<(TempDir, PathBuf)> {
        let temp_dir = TempDir::new()?;
        let test_dir = temp_dir.path().join("test");
        fs::create_dir(&test_dir)?;

        // Create a test JavaScript file
        let test_file = test_dir.join("test.js");
        fs::write(&test_file, "console.log('Hello from test!');")?;

        Ok((temp_dir, test_file))
    }

    #[tokio::test]
    async fn test_run_js_without_chroot() -> Result<()> {
        let (_temp_dir, test_file) = setup_test_env().await?;
        
        let runjs = RunJs::new_default();
        runjs.run_file(test_file.to_str().unwrap()).await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_run_js_with_chroot() -> Result<()> {
        let (temp_dir, test_file) = setup_test_env().await?;
        
        let config = RunJsConfig {
            chroot_path: Some(temp_dir.path().to_path_buf()),
        };
        let runjs = RunJs::new(config);
        
        // Should work with file inside chroot
        runjs.run_file(test_file.to_str().unwrap()).await?;
        
        // Should fail with file outside chroot
        let outside_file = temp_dir.path().join("../outside.js");
        fs::write(&outside_file, "console.log('Outside!');")?;
        
        let result = runjs.run_file(outside_file.to_str().unwrap()).await;
        assert!(result.is_err());
        
        Ok(())
    }

    #[tokio::test]
    async fn test_file_operations() -> Result<()> {
        let (temp_dir, _) = setup_test_env().await?;
        
        let config = RunJsConfig {
            chroot_path: Some(temp_dir.path().to_path_buf()),
        };
        let runjs = RunJs::new(config);
        
        // Create a test file that uses file operations
        let test_file = temp_dir.path().join("file_ops.js");
        fs::write(
            &test_file,
            r#"
            const testFile = './test.txt';
            const content = 'Hello, World!';
            
            // Write file
            await runjs.writeFile(testFile, content);
            
            // Read file
            const readContent = await runjs.readFile(testFile);
            console.log(readContent);
            
            // Remove file
            await runjs.removeFile(testFile);
            "#,
        )?;
        
        runjs.run_file(test_file.to_str().unwrap()).await?;
        
        // Verify file was removed
        assert!(!temp_dir.path().join("test.txt").exists());
        
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch() -> Result<()> {
        let (temp_dir, _) = setup_test_env().await?;
        
        let runjs = RunJs::new_default();
        
        // Create a test file that uses fetch
        let test_file = temp_dir.path().join("fetch_test.js");
        fs::write(
            &test_file,
            r#"
            const response = await runjs.fetch('https://httpbin.org/get');
            console.log(response);
            "#,
        )?;
        
        runjs.run_file(test_file.to_str().unwrap()).await?;
        
        Ok(())
    }
} 