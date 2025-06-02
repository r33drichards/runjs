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
use std::cell::RefCell;
use std::thread_local;

/// Configuration for the RunJS runtime
#[derive(Debug, Clone, Default)]
pub struct RunJsConfig {
    /// The root path for chroot operations. If None, chroot is disabled.
    pub chroot_path: Option<PathBuf>,
}

/// The main RunJS runtime instance
pub struct RunJs {
    config: RunJsConfig,
    chroot_config: Option<ChrootConfig>,
}

thread_local! {
    static CURRENT_RUNJS: RefCell<Option<RunJs>> = const { RefCell::new(None) };
}

impl RunJs {
    /// Create a new RunJS instance with the given configuration
    pub fn new(config: RunJsConfig) -> Self {
        Self { 
            config,
            chroot_config: None,
        }
    }

    /// Create a new RunJS instance with default configuration (no chroot)
    pub fn new_default() -> Self {
        Self::new(RunJsConfig::default())
    }

    // Run a Javascript/Typescript string 
    pub async fn run_string(&mut self, code: &str) -> Result<(), CoreError> {
        // Initialize chroot if enabled
        if let Some(chroot_path) = &self.config.chroot_path {
            let chroot_path = chroot_path.canonicalize().map_err(|e| {
                CoreError::from(JsErrorBox::type_error(format!(
                    "Failed to canonicalize chroot path: {}",
                    e
                )))
            })?;
            
            // Create a ChrootConfig for validation
            let config = ChrootConfig::new(chroot_path.clone());
            self.chroot_config = Some(config);
        }

        // Store self in thread local storage
        CURRENT_RUNJS.with(|runjs| {
            *runjs.borrow_mut() = Some(self.clone());
        });

        // Create a virtual module specifier for the string code
        let specifier = deno_core::resolve_url("data:text/javascript,code.js")
            .map_err(JsErrorBox::from_err)?;

        let module_loader = Rc::new(StringModuleLoader {
            code: code.to_string(),
            specifier: specifier.clone(),
        });

        let mut js_runtime = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
            module_loader: Some(module_loader),
            extensions: vec![runjs::init()],
            ..Default::default()
        });

        // Load the module
        let mod_id = js_runtime.load_main_es_module(&specifier).await?;
        let result = js_runtime.mod_evaluate(mod_id);
        js_runtime.run_event_loop(Default::default()).await?;
        result.await
    }

    /// Run a JavaScript/TypeScript file
    pub async fn run_file(&mut self, file_path: &str) -> Result<(), CoreError> {
        // First validate the path if chroot is enabled
        if let Some(chroot_path) = &self.config.chroot_path {
            let chroot_path = chroot_path.canonicalize().map_err(|e| {
                CoreError::from(JsErrorBox::type_error(format!(
                    "Failed to canonicalize chroot path: {}",
                    e
                )))
            })?;
            
            // Create a temporary ChrootConfig to validate the path
            let config = ChrootConfig::new(chroot_path.clone());
            if let Err(e) = config.validate_path(file_path) {
                return Err(CoreError::from(JsErrorBox::type_error(format!(
                    "File path not allowed in chroot: {}",
                    e
                ))));
            }
            
            self.chroot_config = Some(config);
        }

        let main_module = deno_core::resolve_path(file_path, std::env::current_dir()?.as_path())
            .map_err(JsErrorBox::from_err)?;

        // Store self in thread local storage
        CURRENT_RUNJS.with(|runjs| {
            *runjs.borrow_mut() = Some(self.clone());
        });

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

// Make RunJs cloneable
impl Clone for RunJs {
    fn clone(&self) -> Self {
        Self {
            config: self.config.clone(),
            chroot_config: self.chroot_config.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct ChrootConfig {
    root_path: PathBuf,
}

impl ChrootConfig {
    fn new(root_path: PathBuf) -> Self {
        Self { root_path }
    }

    fn validate_path(&self, path: &str) -> Result<PathBuf, std::io::Error> {
        // First normalize the input path
        let path = Path::new(path);
        let normalized = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root_path.join(path)
        };

        // For new files, validate the parent directory is within chroot
        if !normalized.exists() {
            if let Some(parent) = normalized.parent() {
                if !parent.starts_with(&self.root_path) {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "Path escapes chroot directory",
                    ));
                }
            }
            return Ok(normalized);
        }

        // For existing files, canonicalize and validate
        let canonical = normalized.canonicalize()?;
        if !canonical.starts_with(&self.root_path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Path escapes chroot directory",
            ));
        }
        Ok(canonical)
    }
}

#[op2(async)]
#[string]
async fn op_read_file(
    #[string] path: String,
) -> Result<String, std::io::Error> {
    let path = CURRENT_RUNJS.with(|runjs| {
        let runjs = runjs.borrow();
        let config = runjs.as_ref().and_then(|r| r.chroot_config.as_ref()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Chroot not initialized",
            )
        })?;
        
        config.validate_path(&path)
    })?;
    
    tokio::fs::read_to_string(path).await
}

#[op2(async)]
async fn op_write_file(
    #[string] path: String,
    #[string] contents: String,
) -> Result<(), std::io::Error> {
    let (path, root_path) = CURRENT_RUNJS.with(|runjs| -> Result<(PathBuf, PathBuf), std::io::Error> {
        let runjs = runjs.borrow();
        let config = runjs.as_ref().and_then(|r| r.chroot_config.as_ref()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Chroot not initialized",
            )
        })?;
        
        let path = config.validate_path(&path)?;
        Ok((path, config.root_path.clone()))
    })?;
    
    // Ensure parent directory exists and is within chroot
    if let Some(parent) = path.parent() {
        if !parent.starts_with(&root_path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Parent directory escapes chroot",
            ));
        }
        tokio::fs::create_dir_all(parent).await?;
    }
    
    tokio::fs::write(path, contents).await
}

#[op2(fast)]
fn op_remove_file(
    #[string] path: String,
) -> Result<(), std::io::Error> {
    let path = CURRENT_RUNJS.with(|runjs| {
        let runjs = runjs.borrow();
        let config = runjs.as_ref().and_then(|r| r.chroot_config.as_ref()).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "Chroot not initialized",
            )
        })?;
        
        config.validate_path(&path)
    })?;
    
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
            
            // Validate path against chroot if enabled
            if let Some(config) = CURRENT_RUNJS.with(|runjs| {
                runjs.borrow()
                    .as_ref()
                    .and_then(|r| r.chroot_config.as_ref())
                    .cloned()
            }) {
                if let Err(e) = config.validate_path(path.to_str().unwrap()) {
                    return Err(ModuleLoaderError::from(JsErrorBox::type_error(format!(
                        "Module path not allowed in chroot: {}",
                        e
                    ))));
                }
            }

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

struct StringModuleLoader {
    code: String,
    specifier: deno_core::ModuleSpecifier,
}

impl deno_core::ModuleLoader for StringModuleLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: deno_core::ResolutionKind,
    ) -> Result<deno_core::ModuleSpecifier, ModuleLoaderError> {
        if specifier == self.specifier.as_str() {
            Ok(self.specifier.clone())
        } else {
            deno_core::resolve_import(specifier, referrer).map_err(Into::into)
        }
    }

    fn load(
        &self,
        module_specifier: &deno_core::ModuleSpecifier,
        _maybe_referrer: Option<&reqwest::Url>,
        _is_dyn_import: bool,
        _requested_module_type: deno_core::RequestedModuleType,
    ) -> ModuleLoadResponse {
        if module_specifier == &self.specifier {
            let module = deno_core::ModuleSource::new(
                deno_core::ModuleType::JavaScript,
                deno_core::ModuleSourceCode::String(self.code.clone().into()),
                &self.specifier,
                None,
            );
            ModuleLoadResponse::Sync(Ok(module))
        } else {
            ModuleLoadResponse::Sync(Err(ModuleLoaderError::from(JsErrorBox::type_error(
                "Only the main module is supported for string execution",
            ))))
        }
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
        
        let mut runjs = RunJs::new_default();
        runjs.run_file(test_file.to_str().unwrap()).await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_run_js_with_chroot() -> Result<()> {
        let (temp_dir, test_file) = setup_test_env().await?;
        
        let config = RunJsConfig {
            chroot_path: Some(temp_dir.path().to_path_buf()),
        };
        let mut runjs = RunJs::new(config);
        
        // Should work with file inside chroot
        runjs.run_file(test_file.to_str().unwrap()).await?;
        
        // Should fail with file outside chroot
        let outside_file = temp_dir.path().join("../outside.js");
        fs::write(&outside_file, "console.log('Outside!');")?;
        
        let result = runjs.run_file(outside_file.to_str().unwrap()).await;
        assert!(result.is_err(), "Expected error when accessing file outside chroot");
        
        // Clean up the outside file
        fs::remove_file(outside_file)?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_file_operations() -> Result<()> {
        let (temp_dir, _) = setup_test_env().await?;
        
        let config = RunJsConfig {
            chroot_path: Some(temp_dir.path().to_path_buf()),
        };
        let mut runjs = RunJs::new(config);
        
        // Create a test file that uses file operations
        let test_file = temp_dir.path().join("file_ops.js");
        fs::write(
            &test_file,
            r#"
            const testFile = 'test.txt';  // Use relative path
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
        
        let mut runjs = RunJs::new_default();
        
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

    #[tokio::test]
    async fn test_run_string_basic() -> Result<()> {
        let mut runjs = RunJs::new_default();
        
        // Test basic console.log
        runjs.run_string("console.log('Hello from string!');").await?;
        
        // Test variable declaration and usage
        runjs.run_string(
            r#"
            const x = 42;
            console.log(x * 2);
            "#,
        ).await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_run_string_with_runtime_features() -> Result<()> {
        let mut runjs = RunJs::new_default();
        
        // Test setTimeout
        runjs.run_string(
            r#"
            console.log('Start');
            await setTimeout(100);
            console.log('After timeout');
            "#,
        ).await?;
        
        // Test fetch
        runjs.run_string(
            r#"
            const response = await runjs.fetch('https://httpbin.org/get');
            console.log(response);
            "#,
        ).await?;
        
        Ok(())
    }

    #[tokio::test]
    async fn test_run_string_with_file_operations() -> Result<()> {
        let (temp_dir, _) = setup_test_env().await?;
        
        let config = RunJsConfig {
            chroot_path: Some(temp_dir.path().to_path_buf()),
        };
        let mut runjs = RunJs::new(config);
        
        // Test file operations within chroot
        runjs.run_string(
            r#"
            const testFile = 'test.txt';
            const content = 'Hello from string!';
            
            // Write file
            await runjs.writeFile(testFile, content);
            
            // Read file
            const readContent = await runjs.readFile(testFile);
            console.log(readContent);
            
            // Remove file
            await runjs.removeFile(testFile);
            "#,
        ).await?;
        
        // Verify file was removed
        assert!(!temp_dir.path().join("test.txt").exists());
        
        Ok(())
    }

    #[tokio::test]
    async fn test_run_string_error_handling() -> Result<()> {
        let mut runjs = RunJs::new_default();
        
        // Test syntax error
        let result = runjs.run_string("this is not valid javascript;").await;
        assert!(result.is_err(), "Expected error for invalid JavaScript");
        
        // Test runtime error
        let result = runjs.run_string("throw new Error('Test error');").await;
        assert!(result.is_err(), "Expected error for thrown error");
        
        // Test chroot violation
        let config = RunJsConfig {
            chroot_path: Some(PathBuf::from("/tmp")),
        };
        let mut runjs = RunJs::new(config);
        
        let result = runjs.run_string(
            r#"
            await runjs.writeFile('/etc/test.txt', 'should fail');
            "#,
        ).await;
        assert!(result.is_err(), "Expected error for chroot violation");
        
        Ok(())
    }
} 