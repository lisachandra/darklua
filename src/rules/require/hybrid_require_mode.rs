use core::option::Option::None;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{
    frontend::DarkluaResult,
    nodes::{Arguments, Expression, FunctionCall, Prefix},
    rules::{
        convert_require::{instance_path::InstancePath, rojo_sourcemap::RojoSourcemap},
        parse_roblox, Context,
    }, DarkluaError,
};

use super::PathRequireMode;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct HybridRequireMode {
    #[serde(flatten)]
    path_require_mode: PathRequireMode,

    #[serde(default)]
    convert_ts_imports: bool,

    #[serde(skip)]
    cached_sourcemap: Option<RojoSourcemap>,

    rojo_sourcemap: Option<PathBuf>,
}

impl HybridRequireMode {
    pub(crate) fn initialize(&mut self, context: &Context) -> DarkluaResult<()> {
        if let Some(ref rojo_sourcemap_path) = self
            .rojo_sourcemap
            .as_ref()
            .map(|rojo_sourcemap_path| context.project_location().join(rojo_sourcemap_path))
        {
            context.add_file_dependency(rojo_sourcemap_path.clone());

            let sourcemap_parent_location = get_relative_parent_path(rojo_sourcemap_path);
            let sourcemap = RojoSourcemap::parse(
                &context
                    .resources()
                    .get(rojo_sourcemap_path)
                    .map_err(|err| {
                        DarkluaError::from(err).context("while initializing Roblox require mode")
                    })?,
                sourcemap_parent_location,
            )
            .map_err(|err| {
                err.context(format!(
                    "unable to parse Rojo sourcemap at `{}`",
                    rojo_sourcemap_path.display()
                ))
            })?;
            self.cached_sourcemap = Some(sourcemap);
        }
        Ok(())
    }
}

impl HybridRequireMode {


    pub(crate) fn path_require_mode(&self) -> &PathRequireMode {
        &self.path_require_mode
    }

    pub(crate) fn resolve_require_call(&self, call: &FunctionCall, source: &Path) -> Option<PathBuf> {
        self.resolve_ts_import(call, source)
            .or_else(|| self.resolve_roblox_require(call, source))
    }

    pub(crate) fn resolve_require_path(&self, call: &FunctionCall, source: &Path) -> Option<PathBuf> {
        self.resolve_roblox_require(call, source)
    }

    fn resolve_ts_import(&self, call: &FunctionCall, _source: &Path) -> Option<PathBuf> {
        if !self.convert_ts_imports {
            return None;
        }

        let Prefix::Field(field) = call.get_prefix() else {
            return None;
        };
        match field.get_prefix() {
            Prefix::Identifier(x) if x.get_name() == "TS" && x.get_token().is_none() => Some(()),
            _ => None,
        }?;
        if !(field.get_field().get_name() == "import" && field.get_field().get_token().is_none()) {
            return None;
        }

        let Arguments::Tuple(values) = call.get_arguments() else {
            return None;
        };
        if values.iter_values().count() == 0 {
            return None;
        };

        let mut values_iter = values.iter_values();
        values_iter.next();

        let mut instance_path = InstancePath::from_root();
        if let Some(service_name_expression) = values_iter.next() {
            if let Expression::Call(x) = service_name_expression {
                if let Some(service_name) = extract_service_name(x) {
                    instance_path.child(service_name);
                }
            }
        }
        values_iter.for_each(|expression| {
            if let Expression::String(x) = expression {
                instance_path.child(x.get_string_value().unwrap_or_default());
            }
        });

        if let Some(ref sourcemap) = self.cached_sourcemap {
            if let Some(file_path) = sourcemap.get_file_path(instance_path) {
                return Some(file_path);
            }
        };

        None
    }

    fn resolve_roblox_require(&self, call: &FunctionCall, source: &Path) -> Option<PathBuf> {
        parse_roblox(call, source)
            .ok()
            .flatten()
            .and_then(|x| {
                let mut source_parent = source.to_path_buf();
                source_parent.pop();
                pathdiff::diff_paths(x, source_parent).map(|x| PathBuf::from("./").join(x))
            })
    }
}




fn get_relative_parent_path(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) => {
            if parent == Path::new("") {
                Path::new(".")
            } else {
                parent
            }
        }
        None => Path::new(".."),
    }
}

fn extract_service_name(call: &FunctionCall) -> Option<String> {
    let Some(field) = call.get_method() else {
        return None;
    };

    let Prefix::Identifier(identifier) = call.get_prefix() else {
        return None;
    };

    if identifier.get_name() != "game" {
        return None;
    }

    if field.get_name() != "GetService" {
        return None;
    }

    let Arguments::Tuple(args) = call.get_arguments() else {
        return None;
    };

    if args.iter_values().count() != 1 {
        return None;
    }

    let Some(first_arg) = args.iter_values().next() else {
        return None;
    };

    let Expression::String(service_name) = first_arg else {
        return None;
    };

    Some(service_name.get_string_value().unwrap_or_default().to_string())
}
