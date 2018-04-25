extern crate parity_wasm;
extern crate wasm_bindgen_shared as shared;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate wasm_gc;
extern crate wasmi;

use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use parity_wasm::elements::*;

mod js;
mod descriptor;
pub mod wasm2es6js;

pub struct Bindgen {
    path: Option<PathBuf>,
    nodejs: bool,
    browser: bool,
    no_modules: bool,
    no_modules_global: Option<String>,
    debug: bool,
    typescript: bool,
    demangle: bool,
}

#[derive(Debug)]
pub struct Error(String);

impl<E: std::error::Error> From<E> for Error {
    fn from(e: E) -> Error {
        Error(e.to_string())
    }
}

impl Bindgen {
    pub fn new() -> Bindgen {
        Bindgen {
            path: None,
            nodejs: false,
            browser: false,
            no_modules: false,
            no_modules_global: None,
            debug: false,
            typescript: false,
            demangle: true,
        }
    }

    pub fn input_path<P: AsRef<Path>>(&mut self, path: P) -> &mut Bindgen {
        self.path = Some(path.as_ref().to_path_buf());
        self
    }

    pub fn nodejs(&mut self, node: bool) -> &mut Bindgen {
        self.nodejs = node;
        self
    }

    pub fn browser(&mut self, browser: bool) -> &mut Bindgen {
        self.browser = browser;
        self
    }

    pub fn no_modules(&mut self, no_modules: bool) -> &mut Bindgen {
        self.no_modules = no_modules;
        self
    }

    pub fn no_modules_global(&mut self, name: &str) -> &mut Bindgen {
        self.no_modules_global = Some(name.to_string());
        self
    }

    pub fn debug(&mut self, debug: bool) -> &mut Bindgen {
        self.debug = debug;
        self
    }

    pub fn typescript(&mut self, typescript: bool) -> &mut Bindgen {
        self.typescript = typescript;
        self
    }

    pub fn demangle(&mut self, demangle: bool) -> &mut Bindgen {
        self.demangle = demangle;
        self
    }

    pub fn generate<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        self._generate(path.as_ref())
    }

    fn _generate(&mut self, out_dir: &Path) -> Result<(), Error> {
        let input = match self.path {
            Some(ref path) => path,
            None => panic!("must have a path input for now"),
        };
        let stem = input.file_stem().unwrap().to_str().unwrap();
        let mut module = parity_wasm::deserialize_file(input)?;
        let programs = extract_programs(&mut module);

        // Here we're actually instantiating the module we've parsed above for
        // execution. Why, you might be asking, are we executing wasm code? A
        // good question!
        //
        // Transmitting information from `#[wasm_bindgen]` here to the CLI tool
        // is pretty tricky. Specifically information about the types involved
        // with a function signature (especially generic ones) can be hefty to
        // translate over. As a result, the macro emits a bunch of shims which,
        // when executed, will describe to us what the types look like.
        //
        // This means that whenever we encounter an import or export we'll
        // execute a shim function which informs us about its type so we can
        // then generate the appropriate bindings.
        let instance = wasmi::Module::from_parity_wasm_module(module.clone())?;
        let instance = wasmi::ModuleInstance::new(&instance, &MyResolver)?;
        let instance = instance.not_started_instance();

        let (js, ts) = {
            let mut cx = js::Context {
                globals: String::new(),
                imports: String::new(),
                footer: String::new(),
                typescript: format!("/* tslint:disable */\n"),
                exposed_globals: Default::default(),
                required_internal_exports: Default::default(),
                imported_names: Default::default(),
                exported_classes: Default::default(),
                config: &self,
                module: &mut module,
                function_table_needed: false,
                module_versions: Default::default(),
                run_descriptor: &|name| {
                    let mut v = MyExternals(Vec::new());
                    let ret = instance
                        .invoke_export(name, &[], &mut v)
                        .expect("failed to run export");
                    assert!(ret.is_none());
                    v.0
                },
            };
            for program in programs.iter() {
                js::SubContext {
                    program,
                    cx: &mut cx,
                }.generate();
            }
            cx.finalize(stem)
        };

        let js_path = out_dir.join(stem).with_extension("js");
        File::create(&js_path).unwrap()
            .write_all(js.as_bytes()).unwrap();

        if self.typescript {
            let ts_path = out_dir.join(stem).with_extension("d.ts");
            File::create(&ts_path).unwrap()
                .write_all(ts.as_bytes()).unwrap();
        }

        let wasm_path = out_dir.join(format!("{}_bg", stem)).with_extension("wasm");

        if self.nodejs {
            let js_path = wasm_path.with_extension("js");
            let shim = self.generate_node_wasm_import(&module, &wasm_path);
            File::create(&js_path)?.write_all(shim.as_bytes())?;
        }

        let wasm_bytes = parity_wasm::serialize(module).map_err(|e| {
            Error(format!("{:?}", e))
        })?;
        File::create(&wasm_path)?.write_all(&wasm_bytes)?;
        Ok(())
    }

    fn generate_node_wasm_import(&self, m: &Module, path: &Path) -> String {
        let mut imports = BTreeSet::new();
        if let Some(i) = m.import_section() {
            for i in i.entries() {
                imports.insert(i.module());
            }
        }

        let mut shim = String::new();
        shim.push_str("let imports = {};\n");
        for module in imports {
            shim.push_str(&format!("imports['{0}'] = require('{0}');\n", module));
        }

        shim.push_str(&format!("
            const join = require('path').join;
            const bytes = require('fs').readFileSync(join(__dirname, '{}'));
            const wasmModule = new WebAssembly.Module(bytes);
            const wasmInstance = new WebAssembly.Instance(wasmModule, imports);
            module.exports = wasmInstance.exports;
        ", path.file_name().unwrap().to_str().unwrap()));

        shim
    }
}

fn extract_programs(module: &mut Module) -> Vec<shared::Program> {
    let version = shared::version();
    let mut ret = Vec::new();

    module.sections_mut().retain(|s| {
        let custom = match *s {
            Section::Custom(ref s) => s,
            _ => return true,
        };
        if custom.name() != "__wasm_bindgen_unstable" {
            return true
        }

        let mut payload = custom.payload();
        while payload.len() > 0 {
            let len =
                ((payload[0] as usize) << 0) |
                ((payload[1] as usize) << 8) |
                ((payload[2] as usize) << 16) |
                ((payload[3] as usize) << 24);
            let (a, b) = payload[4..].split_at(len as usize);
            payload = b;
            let p: shared::ProgramOnlySchema = match serde_json::from_slice(&a) {
                Ok(f) => f,
                Err(e) => {
                    panic!("failed to decode what looked like wasm-bindgen data: {}", e)
                }
            };
            if p.schema_version != shared::SCHEMA_VERSION {
                panic!("

it looks like the Rust project used to create this wasm file was linked against
a different version of wasm-bindgen than this binary:

  rust wasm file: {}
     this binary: {}

Currently the bindgen format is unstable enough that these two version must
exactly match, so it's required that these two version are kept in sync by
either updating the wasm-bindgen dependency or this binary. You should be able
to update the wasm-bindgen dependency with:

    cargo update -p wasm-bindgen

or you can update the binary with

    cargo install -f wasm-bindgen-cli

if this warning fails to go away though and you're not sure what to do feel free
to open an issue at https://github.com/alexcrichton/wasm-bindgen/issues!
",
    p.version, version);
            }
            let p: shared::Program = match serde_json::from_slice(&a) {
                Ok(f) => f,
                Err(e) => {
                    panic!("failed to decode what looked like wasm-bindgen data: {}", e)
                }
            };
            ret.push(p);
        }

        false
    });
    return ret
}

struct MyResolver;

impl wasmi::ImportResolver for MyResolver {
    fn resolve_func(
        &self,
        module_name: &str,
        field_name: &str,
        signature: &wasmi::Signature
    ) -> Result<wasmi::FuncRef, wasmi::Error> {
        // Route our special "describe" export to 1 and everything else to 0.
        // That way whenever the function 1 is invoked we know what to do and
        // when 0 is invoked (by accident) we'll trap and produce an error.
        let idx = (module_name == "__wbindgen_placeholder__" &&
            field_name == "__wbindgen_describe") as usize;
        Ok(wasmi::FuncInstance::alloc_host(signature.clone(), idx))
    }

    fn resolve_global(
        &self,
        _module_name: &str,
        _field_name: &str,
        descriptor: &wasmi::GlobalDescriptor
    ) -> Result<wasmi::GlobalRef, wasmi::Error> {
        // dummy implementation to ensure instantiation succeeds
        let val = match descriptor.value_type() {
            wasmi::ValueType::I32 => wasmi::RuntimeValue::I32(0),
            wasmi::ValueType::I64 => wasmi::RuntimeValue::I64(0),
            wasmi::ValueType::F32 => wasmi::RuntimeValue::F32(0.0),
            wasmi::ValueType::F64 => wasmi::RuntimeValue::F64(0.0),
        };
        Ok(wasmi::GlobalInstance::alloc(val, descriptor.is_mutable()))
    }

    fn resolve_memory(
        &self,
        _module_name: &str,
        _field_name: &str,
        descriptor: &wasmi::MemoryDescriptor,
    ) -> Result<wasmi::MemoryRef, wasmi::Error> {
        // dummy implementation to ensure instantiation succeeds
        use wasmi::memory_units::Pages;
        let initial = Pages(descriptor.initial() as usize);
        let maximum = descriptor.maximum().map(|i| Pages(i as usize));
        wasmi::MemoryInstance::alloc(initial, maximum)
    }

    fn resolve_table(
        &self,
        _module_name: &str,
        _field_name: &str,
        descriptor: &wasmi::TableDescriptor
    ) -> Result<wasmi::TableRef, wasmi::Error> {
        // dummy implementation to ensure instantiation succeeds
        let initial = descriptor.initial();
        let maximum = descriptor.maximum();
        wasmi::TableInstance::alloc(initial, maximum)
    }
}

struct MyExternals(Vec<u32>);
#[derive(Debug)]
struct MyError(String);

impl wasmi::Externals for MyExternals {
    fn invoke_index(
        &mut self,
        index: usize,
        args: wasmi::RuntimeArgs
    ) -> Result<Option<wasmi::RuntimeValue>, wasmi::Trap> {
        macro_rules! bail {
            ($($t:tt)*) => ({
                let s = MyError(format!($($t)*));
                return Err(wasmi::Trap::new(wasmi::TrapKind::Host(Box::new(s))))
            })
        }
        // We only recognize one function here which was mapped to the index 1
        // by the resolver above.
        if index != 1 {
            bail!("only __wbindgen_describe can be run at this time")
        }
        if args.len() != 1 {
            bail!("must have exactly one argument");
        }
        match args.nth_value_checked(0)? {
            wasmi::RuntimeValue::I32(i) => self.0.push(i as u32),
            _ => bail!("expected one argument of i32 type"),
        }
        Ok(None)
    }
}

impl wasmi::HostError for MyError {}

impl fmt::Display for MyError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.fmt(f)
    }
}
